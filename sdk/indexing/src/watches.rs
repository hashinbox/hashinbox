//! One network's [`Watch`] rows: the hot set a pass reads, the admission that
//! orders a new watch against a commit, and the durable mirror.
//!
//! The set stays in memory because [`CommitGate::plan`] captures it
//! through a *synchronous* closure and holds no guard across an await, while
//! [`WatchStore::all`] is async. The store is therefore the mirror, read once
//! before the first pass, not the authority a pass consults (`docs/design/030 §8 (ex-029)`
//! section 5.3). [`Watches::with`] and [`Watches::with_mut`] are the only access
//! to the set and both take a synchronous closure, so that is a property of the
//! signatures and not of the code that happens to call them.
//!
//! The store is not held as a field: every method that needs it takes it, so a
//! [`crate::Indexer`] can be assembled without one and the set has no opinion
//! about which of the repository's capabilities reached it.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

pub(crate) use crate::watch_set::OwedRange;
use crate::watch_set::WatchRows;

use crate::{
    Birthday, BlockRef, CommitGate, Holder, IndexError, IndexErrorKind, Network, Progress,
    SyncPlan, SyncPolicy, Watch, WatchStore,
};

/// The collection of what one network observes.
pub(crate) struct Watches {
    network: Network,
    admission: Arc<CommitGate>,
    set: RwLock<WatchRows>,
    policy: SyncPolicy,
    loaded: AtomicBool,
}

impl Watches {
    /// An empty set for `network`.
    pub(crate) fn new(network: Network, policy: SyncPolicy) -> Self {
        Self {
            network,
            admission: Arc::new(CommitGate::new()),
            set: RwLock::new(WatchRows::default()),
            policy,
            loaded: AtomicBool::new(false),
        }
    }

    /// Fills the set from `store`, once, before anything reads it.
    ///
    /// **One read, so one snapshot.** [`WatchStore::all`] hands back each
    /// observation beside the progress recorded for it, and this is where that
    /// pairing is taken over: the set owns it from here on.
    ///
    /// Merge-only for the rows and authoritative for the progress
    /// ([`WatchRows::restore`]), which is also what makes it safe to run again after
    /// [`Self::mark_stale`].
    pub(crate) async fn load(&self, store: &dyn WatchStore) -> Result<(), IndexError> {
        if self.loaded.load(Ordering::Acquire) {
            return Ok(());
        }
        let stored = store.all(&self.network).await?;
        self.with_mut(|set| set.restore(stored))?;
        self.loaded.store(true, Ordering::Release);
        Ok(())
    }

    /// Marks the mirror as out of date, so the next [`Self::load`] reads it
    /// again.
    ///
    /// Called when a store refused a write the set's own belief authorised: a
    /// batch whose outcome was lost leaves the store ahead of this set, and the
    /// refusal is the only way to find out. Re-reading is what turns a permanent
    /// refusal into one retryable pass (`docs/design/030 §8 (ex-032)`, the expected-progress
    /// guard).
    pub(crate) fn mark_stale(&self) {
        self.loaded.store(false, Ordering::Release);
    }

    /// Every observation still reading its own past ([`WatchRows::behind`]).
    pub(crate) fn behind(
        &self,
        checkpoint: Option<&BlockRef>,
    ) -> Result<Vec<OwedRange>, IndexError> {
        self.with(|set| set.behind(checkpoint))
    }

    /// The claims one committed block proves ([`WatchRows::forward`]).
    pub(crate) fn forward(
        &self,
        line: Option<&BlockRef>,
        block: &BlockRef,
    ) -> Result<Vec<Progress>, IndexError> {
        self.with(|set| set.forward(line, block))
    }

    /// Records what a committed batch proved about each observation it advanced.
    ///
    /// The blocks and their claims commit in one store transaction, so what
    /// reaches here is durable: the set is the owner, and it now holds an answer
    /// a restart will find.
    pub(crate) fn advanced(&self, proved: &[Progress]) -> Result<(), IndexError> {
        self.with_mut(|set| set.advanced(proved))
    }

    /// The captured plan for this pass: filters, checkpoint and revision
    /// together, under the admission handshake ([`WatchRows::filters`]).
    pub(crate) fn plan(&self, checkpoint: Option<BlockRef>) -> Result<SyncPlan, IndexError> {
        self.admission
            .plan(checkpoint, |line| self.with(|set| set.filters(line)))
    }

    /// Every row, in id order, as [`crate::Watching::watched`] returns them.
    pub(crate) fn rows(&self) -> Result<Vec<Watch>, IndexError> {
        self.with(WatchRows::watches)
    }

    /// Rejects a network this set does not hold.
    pub(crate) fn owns(&self, network: &Network) -> Result<(), IndexError> {
        if *network == self.network {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::NetworkMismatch,
            "index network is not configured",
            false,
        ))
    }

    /// Adds one watch: the durable row first, then the hot set, then the
    /// revision bump that invalidates any plan captured before it.
    ///
    /// `persisted` is the checkpoint the caller just read; the permit's own
    /// checkpoint is the one a `Birthday::Fresh` birthday is derived from, and no
    /// block may move past it while the permit is held.
    pub(crate) async fn add(
        &self,
        store: &dyn WatchStore,
        id: &str,
        subject: &dyn Holder,
        start: Birthday,
        persisted: Option<BlockRef>,
    ) -> Result<Watch, IndexError> {
        self.validate(subject)?;
        let permit = self.admission.publication(persisted).await?;
        if let Some(existing) = self.existing(id, subject, start)? {
            return Ok(existing);
        }
        let watch = Watch {
            id: id.to_owned(),
            address: subject.address().clone(),
            asset: subject.asset().clone(),
            start: Self::birthday(start, permit.checkpoint())?,
        };
        // Refused here, synchronously, under the permit that fixed the
        // checkpoint the answer is relative to. A start the index cannot
        // afford to honour must fail the call that asked for it, not a
        // background pass an hour later (`docs/design/024` section 7).
        if let Some(line) = permit.checkpoint() {
            self.policy.admits(watch.start, line.position)?;
        }
        // Durable first: a crash after it leaves an observation the next start
        // restores, never a hot row nothing remembers. One entry, through the
        // batch port: a caller registering a wallet's native currency and its
        // tokens together passes them in one `add` and gets all or none.
        let mut uncertain = self.uncertain();
        uncertain.writing(id);
        store.add(vec![watch.clone()]).await?;
        self.with_mut(|set| set.attach(watch.clone()))?;
        permit.complete()?;
        uncertain.settled();
        Ok(watch)
    }

    /// Takes one watch out: the durable rows first, then the hot set, then the
    /// revision bump that invalidates any plan captured before it.
    ///
    /// The contract is [`crate::Watching::unwatch`]; what is decided here is the
    /// same handshake [`Self::add`] takes, and deliberately not a second one.
    pub(crate) async fn remove(
        &self,
        store: &dyn WatchStore,
        id: &str,
        persisted: Option<BlockRef>,
    ) -> Result<Option<Watch>, IndexError> {
        let permit = self.admission.publication(persisted).await?;
        let Some(watch) = self.with(|set| set.identified(id))? else {
            // Nothing was watched, so nothing is published: the permit is
            // released without bumping the revision, and no pass is disturbed.
            return Ok(None);
        };
        // The store removes the observation and the progress recorded for it in
        // one write, so the pair cannot come apart. A claim left behind by a
        // forgotten observation could be inherited by a re-watch and would then
        // cover a range committed while nothing watched the address.
        let mut uncertain = self.uncertain();
        uncertain.writing(&watch.id);
        store.forget(&self.network, &watch).await?;
        self.with_mut(|set| set.detach(&watch.id))?;
        permit.complete()?;
        uncertain.settled();
        Ok(Some(watch))
    }

    /// Stops asking for every range this network still owes, and reports the
    /// watches it re-anchored.
    ///
    /// The contract is [`crate::Backfilling::cancel_backfill`]. Each affected observation
    /// is re-anchored to the block after the checkpoint with **nothing inspected
    /// for it**, which is the one honest way to settle a range without claiming
    /// to have read it: a watch born above the line owes nothing below it, and
    /// `indexed_through` stays `None` until a block is actually inspected.
    /// Nothing indexed is deleted, and no coverage note has to be written at all
    /// — which is the machinery contiguous progress removed here.
    ///
    /// **One durable call, and that is a property of [`WatchStore::cancel_backfill`]
    /// rather than of this function.** Raising a birthday used to be a
    /// [`WatchStore::forget`] followed by a [`WatchStore::add`], because nothing
    /// else could change one; those two pass through a state in which the
    /// address is watched by nothing, and a crash there leaves it there. So this
    /// takes no reconciliation obligation ([`Reconciliation`]) where
    /// [`Self::add`] and [`Self::remove`] must: no row appears or disappears
    /// here, and the only divergence a lost acknowledgement can leave is a hot
    /// birthday *below* the durable one — the set still owing a range the store
    /// has renounced, which reads honestly, re-anchors again on a repeat of the
    /// same idempotent call, and is refused by the expected-progress guard the
    /// moment it tries to claim anything (`crate::Progress`).
    pub(crate) async fn cancel_backfill(
        &self,
        store: &dyn WatchStore,
        persisted: Option<BlockRef>,
    ) -> Result<Vec<Watch>, IndexError> {
        let permit = self.admission.publication(persisted).await?;
        let Some(checkpoint) = permit.checkpoint().cloned() else {
            // Nothing is owed below a line the network does not have.
            return Ok(Vec::new());
        };
        let birthday = Self::birthday(Birthday::Fresh, Some(&checkpoint))?;
        let renewed = self
            .owing(&checkpoint)?
            .into_iter()
            .map(|watch| Watch {
                start: birthday,
                ..watch
            })
            .collect::<Vec<_>>();
        if renewed.is_empty() {
            return Ok(Vec::new());
        }
        store.cancel_backfill(renewed.clone()).await?;
        self.with_mut(|set| {
            for watch in &renewed {
                set.attach(watch.clone());
            }
        })?;
        permit.complete()?;
        Ok(renewed)
    }

    /// A reconciliation obligation for the rows a call is about to write.
    fn uncertain(&self) -> Reconciliation<'_> {
        Reconciliation {
            watches: self,
            rows: Vec::new(),
        }
    }

    /// Every watch that still owes a range at `checkpoint`.
    fn owing(&self, checkpoint: &BlockRef) -> Result<Vec<Watch>, IndexError> {
        self.with(|set| {
            set.behind(Some(checkpoint))
                .into_iter()
                .filter_map(|owed| set.identified(&owed.watch_id))
                .collect()
        })
    }

    /// The row this call re-states, or the conflict it collides with.
    ///
    /// **A second asset at a watched address is not a collision.** The durable
    /// list is keyed by `(address, asset)` and always was; the hot set used to
    /// key by address alone and refused the pair here, which is why the
    /// capability the store supports could not be reached and why a restart
    /// silently dropped the second row (`docs/design/030 §8 (ex-032)` section 4). What is
    /// refused is a name that is taken: the same identity over another
    /// observation, or the same `(address, asset)` under another identity.
    ///
    /// Two short reads rather than one held lock: the publication permit
    /// [`Self::add`] holds already excludes every other registration.
    fn existing(
        &self,
        id: &str,
        subject: &dyn Holder,
        start: Birthday,
    ) -> Result<Option<Watch>, IndexError> {
        let restated = |watch: &Watch| {
            watch.address == *subject.address()
                && watch.asset == *subject.asset()
                && !matches!(start, Birthday::At(position) if position != watch.start)
        };
        if let Some(watch) = self.with(|set| set.identified(id))? {
            return if restated(&watch) {
                Ok(Some(watch))
            } else {
                Err(Self::conflict("this watch id is already registered"))
            };
        }
        let taken = self.with(|set| set.holder(subject.address(), subject.asset()))?;
        if let Some(holder) = taken {
            return Err(Self::conflict(if holder.id == id {
                "this watch id is already registered"
            } else {
                "this address is already watched for this asset"
            }));
        }
        Ok(None)
    }

    fn birthday(
        start: Birthday,
        checkpoint: Option<&BlockRef>,
    ) -> Result<crate::BlockPosition, IndexError> {
        match (start, checkpoint) {
            (Birthday::At(position), _) => Ok(position),
            (Birthday::Fresh, None) => Ok(crate::BlockPosition(0)),
            (Birthday::Fresh, Some(block)) => block.position.checked_successor().ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "checkpoint position has no successor for a new watch",
                    false,
                )
            }),
        }
    }

    fn validate(&self, subject: &dyn Holder) -> Result<(), IndexError> {
        let address = subject.address();
        if !address.belongs_to(&self.network) {
            return Err(IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "watched address belongs to another network",
                false,
            ));
        }
        if address.value.is_empty() || subject.asset().chain != self.network.chain {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "a watched address must be non-empty and read in its own chain's asset",
                false,
            ));
        }
        Ok(())
    }

    fn conflict(message: &'static str) -> IndexError {
        IndexError::new(IndexErrorKind::Conflict, message, false)
    }

    /// Reads the hot set under the lock; [`Self::with_mut`] changes it.
    ///
    /// **Both closures are synchronous, and that is the invariant.** Neither is
    /// an `async fn`, neither takes or returns a future, and neither can return
    /// the guard — a borrow of the [`WatchRows`] does not outlive the call — so no guard
    /// is ever held across an await and no pass observes the set half-published.
    /// These two are the *only* access to `self.set`, which puts that property in
    /// their signatures instead of in the code that calls them: there is no way
    /// here to obtain a guard, so there is nothing for an `.await` to be written
    /// after (`docs/design/030 §8 (ex-029)` section 5.3, `docs/design/033` section 5.7).
    fn with<T>(&self, read: impl FnOnce(&WatchRows) -> T) -> Result<T, IndexError> {
        let set = self.set.read().map_err(|_| Self::poisoned())?;
        Ok(read(&set))
    }

    /// Changes the hot set under the lock, under [`Self::with`]'s invariant.
    fn with_mut<T>(&self, change: impl FnOnce(&mut WatchRows) -> T) -> Result<T, IndexError> {
        let mut set = self.set.write().map_err(|_| Self::poisoned())?;
        Ok(change(&mut set))
    }

    fn poisoned() -> IndexError {
        IndexError::new(
            IndexErrorKind::Store,
            "the watch set lock is poisoned",
            false,
        )
    }
}

/// The rows a registration or a withdrawal may have left in an unknown durable
/// state.
///
/// Those two, and no others, because those two are the calls that make a row
/// appear or disappear. [`Watches::cancel_backfill`] changes a row that exists into a row
/// that exists ([`WatchStore::cancel_backfill`]), so it has no outcome in which the
/// store holds a row the running set has never seen — which is the case this
/// whole mechanism is about.
///
/// [`WatchStore::add`] states the obligation: "a storage error or a cancelled
/// future can still leave the outcome uncertain — retry rather than assuming
/// rollback". A future cancelled at one of those awaits runs no code after it —
/// the call does not return, it stops existing — so a drop guard is the only
/// place that obligation can be recorded, and dropping the publication permit
/// alone recorded nothing: the store held a row the running set had never seen,
/// and nothing would ever look again.
///
/// So the outcome is settled the one way it can be: **read it back**. Each named
/// row leaves the hot set and the mirror is marked stale
/// ([`Watches::mark_stale`]), so the next [`Watches::load`] — the first thing every
/// pass and every `Watching` call does — re-reads [`WatchStore::all`] and
/// [`WatchRows::restore`] inserts whichever of those rows the store actually holds,
/// whole, with its birthday and its progress, because the id is absent. That is
/// the same authoritative-reload path a refused commit already takes, and
/// detaching first is what makes a merge-only restore authoritative for exactly
/// these rows and no others.
///
/// It fires on an error as well as on a cancellation. A definite refusal wrote
/// nothing, so reconciling it is one wasted read of a list bounded by the watch
/// count; from outside the port there is no way to tell that case from a write
/// whose acknowledgement was lost, and the two must not be guessed between.
struct Reconciliation<'a> {
    watches: &'a Watches,
    /// The identities whose durable outcome is not yet known.
    rows: Vec<String>,
}

impl Reconciliation<'_> {
    /// Names one row this call is about to write durably.
    fn writing(&mut self, id: &str) {
        self.rows.push(id.to_owned());
    }

    /// The outcome is known, so there is nothing left to reconcile.
    fn settled(mut self) {
        self.rows.clear();
    }
}

impl Drop for Reconciliation<'_> {
    fn drop(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        // A poisoned set is already unusable and reports itself on the next
        // access; there is nothing to do about it from a drop.
        drop(self.watches.with_mut(|set| {
            for id in &self.rows {
                set.detach(id);
            }
        }));
        self.watches.mark_stale();
    }
}
