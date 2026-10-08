//! **Organ.**
//!
//! What the index observes: the durable list, and the selection one pass reads.

use crate::{
    Address, AssetId, BlockPosition, BlockRef, BoxFuture, Budget, Gate, Holder, IndexError,
    IndexErrorKind, Network, Plan, Progress, WatchProgress, WatchStore,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// The range one address is inspected for at the front of the chain: the
/// address, and the first block a pass must inspect for it.
///
/// One item of work, not a predicate, and the forward counterpart of the range
/// an observation still owes below the line. It is the reduced form of a
/// [`Watch`] that a pass actually needs, read from the index's own watch set
/// rather than handed in by a caller. `start_position` is the address's *next
/// required* block — the successor of whatever was last inspected for it, or
/// its birthday when nothing was — and not simply the birthday, which is why
/// this is derived from [`crate::WatchProgress`] rather than from a [`Watch`]
/// alone. The range has no end of its own: it runs to wherever the pass stops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRange {
    /// The address to index, in its network's canonical text.
    pub address: Address,
    /// The first block this pass must inspect for the address: the successor of
    /// whatever was last inspected for it, or its birthday when nothing was.
    pub start_position: BlockPosition,
}

/// Where an address begins to be observed.
///
/// No default and no `Option`: a birthday below the checkpoint was once
/// accepted silently and indexed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Birthday {
    /// This key or address has never been used, so observation begins at the
    /// block after the current checkpoint, or after a verified source tip when
    /// no checkpoint exists. Repeating a registration keeps its original birthday.
    /// An assertion of fact: a caller who does not know cannot honestly write it.
    Fresh,
    /// Observe from this native position.
    At(BlockPosition),
}

/// One observed address as the index holds it: one wallet's address, in one
/// asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    /// The wallet's id, unique across the index. One wallet is one address,
    /// so every watch of it shares this id and differs by asset.
    pub id: String,
    /// The address, and therefore the network.
    pub address: Address,
    /// The asset its history and balance are read in.
    pub asset: AssetId,
    /// The first block inspected for it.
    pub start: BlockPosition,
}

impl Watch {
    /// Rejects a holding no store can hold rows for.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`](crate::IndexErrorKind::InvalidRequest)
    /// for a blank address or an asset of another chain.
    pub fn validate(&self) -> Result<(), IndexError> {
        crate::storage::subject::require_holding(&self.address, &self.asset)
    }
}

/// A stored watch is the holding it observes, under an id and a birthday, so
/// [`Watch::validate`] is the whole of what a store owes its address and asset
/// — the blank address and the foreign asset [`crate::WatchStore::add`]
/// refuses, checked by the row rather than by each store that holds one.
impl Holder for Watch {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &AssetId {
        &self.asset
    }
}

/// The index's durable list of what it observes.
///
/// A watch can be withdrawn. `029` section 5.2 refused an `unwatch` on two
/// grounds, and both are answered: the durable list's method budget was spent on
/// an `advance` that was never built — progress is written in the commit's own
/// transaction instead — and the fate of an unwatched address's committed
/// history is stated on [`Self::unwatch`] rather than left open.
/// Nothing is deleted, so the act is reversible, which is what makes it safe to
/// expose at all.
pub trait Watching: Send + Sync {
    /// Observes `subject`'s address from `start`, durably, under `id`.
    ///
    /// The row is committed before this returns, so a caller that keeps a key
    /// for `id` keeps it *after* this call: a crash then leaves a watched
    /// address with no key, which is a watch-only address, rather than a key
    /// nothing observes.
    ///
    /// `id` names a wallet the wallets side already filed for `subject`'s
    /// address; this adds its observation in `subject`'s asset. Re-watching
    /// the same asset under the same id is a no-op; another `Birthday::At` for
    /// it, a known id on another address, or (in a store that checks it) a
    /// wallet nobody filed, is `Conflict`.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::NetworkMismatch`] for an address of another
    /// network, [`crate::IndexErrorKind::InvalidRequest`] for a blank address
    /// or an asset of another chain, [`crate::IndexErrorKind::Conflict`] for
    /// the collisions above, and the registry's own error.
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn Holder,
        start: Birthday,
    ) -> BoxFuture<'a, Result<Watch, IndexError>>;

    /// Stops observing every asset wallet `id` watches, and returns the
    /// watches it removed.
    ///
    /// **No indexed row is deleted.** The history, the live outputs and the
    /// credits of the address all stay; what stops is future inspection. Rows
    /// nothing claims are the price, and it is the cheaper one: deleting would be
    /// a second removal path beside the offline
    /// [`crate::IndexStore::purge`] and an irreversible one, for an act
    /// that is otherwise entirely reversible. The rows stay readable, because
    /// they are keyed by address and asset and by no watch — which is the same reason
    /// they answer for an address this process watched a moment ago. What the
    /// rows may *claim* goes with the observation: a holding nothing observes
    /// is never read as caught up.
    ///
    /// The one thing it does remove is the *claim*:
    /// [`crate::WatchProgress::indexed_through`]
    /// ([`crate::WatchStore::forget`]). It says every applicable block from this
    /// observation's birthday through that block was inspected for it, and it
    /// holds only while the observation exists — blocks committed while nothing
    /// watched the address were never inspected for it, and no field in the row
    /// tells the two apart. The claim is evidence about an observation, not about
    /// the chain, so it goes with the observation. Another observation of the same
    /// address keeps its own, earned under its own birthday.
    ///
    /// **The address may be watched again**, under its own id — the store keeps
    /// the wallet row — from any [`Birthday`], and it then reads its range again rather than
    /// inheriting a
    /// settled claim: bounded by the same
    /// [`Budget::max_backfill_blocks`](crate::Budget) the first watch was
    /// bounded by, and idempotent where the rows already exist (verified, not
    /// rewritten). Inspection *is* address-scoped and asset-independent by
    /// construction — [`crate::BlockInterpreter::inspect`] takes addresses and no
    /// asset — which is why one read of an address serves every asset observed at
    /// it. What is not inheritable is a claim
    /// across an interruption.
    ///
    /// **It orders against a running pass exactly as [`Self::watch`] does**,
    /// under the same publication permit and with no second mechanism: an
    /// in-flight commit finishes first, no block commits while the row is being
    /// removed, and the plan a pass captured before it fails its next commit
    /// with a retryable `Conflict` — so a chunk mid-inspection for this address
    /// is discarded rather than written after the row is gone.
    ///
    /// **Idempotent.** An `id` nothing watches is an empty list, not an error.
    ///
    /// # Errors
    ///
    /// The registry's own error, and [`crate::IndexErrorKind::Store`] when the
    /// watch set lock is poisoned.
    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>>;

    /// Every address this index observes on `network`, in id order.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::NetworkMismatch`] for a network this index
    /// does not own, and the registry's own error on the first read.
    fn watched<'a>(&'a self, network: &'a Network)
    -> BoxFuture<'a, Result<Vec<Watch>, IndexError>>;
}

// The rows one network's watch set holds: one observation, and the block
// through which it has been inspected.
//
// **One collection, keyed by watch identity.** An observation is what a caller
// registers and withdraws, and how far it has been inspected is a fact about
// that observation, so the two live in one row
// ([`crate::WatchProgress`]) — which is also exactly what
// [`crate::WatchStore::all`] hands back, so nothing here re-pairs anything. A
// wallet holding a chain's own currency and a token at one address is two
// observations with two rows and two claims, and they advance together because
// one inspection of the address serves both
// ([`crate::BlockInterpreter::inspect`] takes addresses and no asset), not
// because they share a row.
//
// Inspection is still deduplicated by *address*, in [`Rows::filters`] and in
// [`Rows::behind`]: a pass fetches and interprets a block once per address
// however many assets are observed at it.

/// One observation's identity: its wallet's id and its asset.
pub(crate) type WatchKey = (String, AssetId);

/// Everything one network's watch set holds, under one lock.
#[derive(Default)]
pub(crate) struct Rows {
    /// Every observation and its progress, by the observation's own identity.
    pub(crate) rows: BTreeMap<WatchKey, WatchProgress>,
}

impl Rows {
    /// The selection a pass inspects the blocks above the confirmed line for,
    /// in address order, **deduplicated**.
    ///
    /// Only observations that are [`caught up`](WatchProgress::caught_up) with
    /// `checkpoint`: one still reading its own past is inspected for the blocks
    /// it owes and not for the ones arriving now, which is the trade that
    /// predicate names and the whole reason one reference can describe a watch's
    /// coverage.
    ///
    /// Each address enters once, from the lowest block any caught-up observation
    /// of it still needs — its [`next_required`](WatchProgress::next_required),
    /// not its birthday, because a caught-up observation resumes after what was
    /// already inspected for it.
    pub(crate) fn filters(&self, checkpoint: Option<&BlockRef>) -> Vec<crate::ForwardRange> {
        let mut earliest: BTreeMap<&Address, BlockPosition> = BTreeMap::new();
        for row in self.rows.values() {
            let Some(next) = row.next_required().filter(|_| row.caught_up(checkpoint)) else {
                continue;
            };
            earliest
                .entry(&row.watch.address)
                .and_modify(|start| *start = (*start).min(next))
                .or_insert(next);
        }
        earliest
            .into_iter()
            .map(|(address, start_position)| crate::ForwardRange {
                address: address.clone(),
                start_position,
            })
            .collect()
    }

    /// Every observation still reading its own past, with what it needs next.
    ///
    /// The complement of [`Self::filters`] under the same predicate, so no
    /// observation is in both and none is in neither: what one pass inspects at
    /// the front and what it fills in below the line are one partition of the
    /// set.
    pub(crate) fn behind(&self, checkpoint: Option<&BlockRef>) -> Vec<Owed> {
        self.rows
            .values()
            .filter(|row| !row.caught_up(checkpoint))
            .filter_map(|row| {
                Some(Owed {
                    watch_id: row.watch.id.clone(),
                    asset: row.watch.asset.clone(),
                    address: row.watch.address.clone(),
                    expected: row.indexed_through.clone(),
                    resume: row.next_required()?,
                })
            })
            .collect()
    }

    /// The claims one committed block proves, for the observations inspecting the
    /// front of the chain.
    ///
    /// `line` is the checkpoint as the pass holds it — the block this one
    /// extends — so the partition [`Self::filters`] captured is the partition
    /// advanced here, and an observation still reading its own past is untouched
    /// however far the line moves.
    ///
    /// A claim carries the value its author believed the store held, which is
    /// what the store compares ([`Progress`]).
    pub(crate) fn forward(&self, line: Option<&BlockRef>, block: &BlockRef) -> Vec<Progress> {
        self.rows
            .values()
            .filter(|row| row.caught_up(line))
            .filter_map(|row| {
                let next = row.next_required()?;
                (next <= block.position).then(|| Progress {
                    watch_id: row.watch.id.clone(),
                    asset: row.watch.asset.clone(),
                    expected: row.indexed_through.clone(),
                    indexed_through: block.clone(),
                })
            })
            .collect()
    }

    /// Takes over what a committed batch proved: the claims are durable now, so
    /// the rows they name hold an answer a restart will find.
    pub(crate) fn advanced(&mut self, proved: &[Progress]) {
        for claim in proved {
            let key = (claim.watch_id.clone(), claim.asset.clone());
            if let Some(row) = self.rows.get_mut(&key) {
                row.indexed_through = Some(claim.indexed_through.clone());
            }
        }
    }

    /// Takes over everything the store holds for this network.
    ///
    /// Merge-only for the observations, so a watch registered while the read was
    /// in flight is not replaced by a row that predates it; **authoritative for
    /// the progress**, because the index only ever advances a row after the
    /// store confirmed the write, so the store's value is this one or newer. A
    /// batch whose outcome was lost is exactly the "newer" case, and reloading is
    /// how the index finds out.
    pub(crate) fn restore(&mut self, stored: Vec<WatchProgress>) {
        for row in stored {
            let key = (row.watch.id.clone(), row.watch.asset.clone());
            match self.rows.get_mut(&key) {
                Some(held) => held.indexed_through = row.indexed_through,
                None => drop(self.rows.insert(key, row)),
            }
        }
    }

    /// Every watch, in id order, as [`crate::Watching::watched`] returns them.
    pub(crate) fn watches(&self) -> Vec<crate::Watch> {
        self.rows.values().map(|row| row.watch.clone()).collect()
    }

    /// Every watch of wallet `id`, one per asset.
    pub(crate) fn identified(&self, id: &str) -> Vec<crate::Watch> {
        self.rows
            .values()
            .map(|row| &row.watch)
            .filter(|watch| watch.id == id)
            .cloned()
            .collect()
    }

    /// The observation that already holds `(address, asset)`, if any.
    pub(crate) fn holder(&self, address: &Address, asset: &crate::AssetId) -> Option<crate::Watch> {
        self.rows
            .values()
            .map(|row| &row.watch)
            .find(|watch| &watch.address == address && &watch.asset == asset)
            .cloned()
    }

    /// Puts `watch` in the set with nothing inspected for it yet.
    ///
    /// Nothing is inherited and nothing is reset. A new observation owes its own
    /// range from its own birthday, and every other observation — including
    /// another one of the same address, registered earlier and possibly from
    /// further back — keeps the claim it earned. That is what per-observation
    /// progress buys over a claim shared by address, where a second observation
    /// from an earlier birthday had to widen the shared range and a restart in
    /// between could read the narrower row back as settled.
    pub(crate) fn attach(&mut self, watch: crate::Watch) {
        let key = (watch.id.clone(), watch.asset.clone());
        drop(self.rows.insert(key, WatchProgress::unread(watch)));
    }

    /// Forgets the observation `key` names, and the progress it earned with it.
    ///
    /// By identity, because that is all a row is keyed by: the same call
    /// settles a watch whose durable removal is confirmed and one whose
    /// removal is merely uncertain (`Reconciliation` in [`crate::watches`]).
    pub(crate) fn detach(&mut self, key: &WatchKey) {
        drop(self.rows.remove(key));
    }
}

/// The range one observation still owes: the address to inspect, and where to
/// resume.
///
/// The reduced form of a [`WatchProgress`] the filling half of a pass needs, and
/// it carries `expected` because the claim it will write has to say what it
/// believed the store held ([`Progress`]).
pub(crate) struct Owed {
    /// The observation's wallet, which the claim is written against.
    pub(crate) watch_id: String,
    /// The observation's asset, the rest of its identity.
    pub(crate) asset: AssetId,
    /// The address to inspect for it.
    pub(crate) address: Address,
    /// The progress this observation believes the store holds.
    pub(crate) expected: Option<BlockRef>,
    /// The first block still owed.
    pub(crate) resume: BlockPosition,
}

// One network's [`Watch`] rows: the hot set a pass reads, the admission that
// orders a new watch against a commit, and the durable mirror.
//
// The set stays in memory because [`Gate::plan`] captures it
// through a *synchronous* closure and holds no guard across an await, while
// [`WatchStore::all`] is async. The store is therefore the mirror, read once
// before the first pass, not the authority a pass consults. [`Watches::with`] and
// [`Watches::with_mut`] are the only access
// to the set and both take a synchronous closure, so that is a property of the
// signatures and not of the code that happens to call them.
//
// The store is not held as a field: every method that needs it takes it, so a
// [`crate::Indexer`] can be assembled without one and the set has no opinion
// about which of the repository's capabilities reached it.

/// The collection of what one network observes.
pub(crate) struct Watches {
    network: Network,
    admission: Arc<Gate>,
    set: RwLock<Rows>,
    policy: Budget,
    loaded: AtomicBool,
}

impl Watches {
    /// An empty set for `network`.
    pub(crate) fn new(network: Network, policy: Budget) -> Self {
        Self {
            network,
            admission: Arc::new(Gate::new()),
            set: RwLock::new(Rows::default()),
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
    /// ([`Rows::restore`]), which is also what makes it safe to run again after
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
    /// refusal into one retryable pass.
    pub(crate) fn mark_stale(&self) {
        self.loaded.store(false, Ordering::Release);
    }

    /// Every observation still reading its own past ([`Rows::behind`]).
    pub(crate) fn behind(&self, checkpoint: Option<&BlockRef>) -> Result<Vec<Owed>, IndexError> {
        self.with(|set| set.behind(checkpoint))
    }

    /// The claims one committed block proves ([`Rows::forward`]).
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
    /// together, under the admission handshake ([`Rows::filters`]).
    pub(crate) fn plan(&self, checkpoint: Option<BlockRef>) -> Result<Plan, IndexError> {
        self.admission
            .plan(checkpoint, |line| self.with(|set| set.filters(line)))
    }

    /// Every row, in id order, as [`crate::Watching::watched`] returns them.
    pub(crate) fn rows(&self) -> Result<Vec<Watch>, IndexError> {
        self.with(Rows::watches)
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
    /// block may move past it while the permit is held. `fallback` is a verified
    /// source tip used only when the permit still has no committed checkpoint;
    /// it never becomes the admission gate's expected checkpoint.
    pub(crate) async fn add(
        &self,
        store: &dyn WatchStore,
        id: &str,
        subject: &dyn Holder,
        start: Birthday,
        persisted: Option<BlockRef>,
        fallback: Option<&BlockRef>,
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
            start: Self::birthday(start, permit.checkpoint().or(fallback))?,
        };
        // Refused here, synchronously, under the permit that fixed the
        // checkpoint the answer is relative to. A start the index cannot
        // afford to honour must fail the call that asked for it, not a
        // background pass an hour later.
        if let Some(line) = permit.checkpoint() {
            self.policy.admits(watch.start, line.position)?;
        }
        // Durable first: a crash after it leaves an observation the next start
        // restores, never a hot row nothing remembers. One entry, through the
        // batch port: a caller registering a wallet's native currency and its
        // tokens together passes them in one `add` and gets all or none.
        let mut uncertain = self.uncertain();
        uncertain.writing((watch.id.clone(), watch.asset.clone()));
        store.add(vec![watch.clone()]).await?;
        self.with_mut(|set| set.attach(watch.clone()))?;
        permit.complete()?;
        uncertain.settled();
        Ok(watch)
    }

    /// Takes every watch of wallet `id` out: the durable rows first, then the
    /// hot set, then the revision bump that invalidates any plan captured
    /// before it.
    ///
    /// The contract is [`crate::Watching::unwatch`]; what is decided here is the
    /// same handshake [`Self::add`] takes, and deliberately not a second one.
    pub(crate) async fn remove(
        &self,
        store: &dyn WatchStore,
        id: &str,
        persisted: Option<BlockRef>,
    ) -> Result<Vec<Watch>, IndexError> {
        let permit = self.admission.publication(persisted).await?;
        let watches = self.with(|set| set.identified(id))?;
        if watches.is_empty() {
            // Nothing was watched, so nothing is published: the permit is
            // released without bumping the revision, and no pass is disturbed.
            return Ok(watches);
        }
        // The store removes the observation and the progress recorded for it in
        // one write, so the pair cannot come apart. A claim left behind by a
        // forgotten observation could be inherited by a re-watch and would then
        // cover a range committed while nothing watched the address.
        let mut uncertain = self.uncertain();
        for watch in &watches {
            uncertain.writing((watch.id.clone(), watch.asset.clone()));
        }
        for watch in &watches {
            store.forget(&self.network, watch).await?;
        }
        self.with_mut(|set| {
            for watch in &watches {
                set.detach(&(watch.id.clone(), watch.asset.clone()));
            }
        })?;
        permit.complete()?;
        uncertain.settled();
        Ok(watches)
    }

    /// A reconciliation obligation for the rows a call is about to write.
    fn uncertain(&self) -> Reconciliation<'_> {
        Reconciliation {
            watches: self,
            rows: Vec::new(),
        }
    }

    /// The row this call re-states, or the conflict it collides with.
    ///
    /// **A second asset of a watched wallet is not a collision.** What is
    /// refused is a name that is taken: the same wallet and asset from another
    /// birthday, the same wallet id over another address, or the same
    /// `(address, asset)` under another wallet id.
    ///
    /// Two short reads rather than one held lock: the publication permit
    /// [`Self::add`] holds already excludes every other registration.
    pub(crate) fn existing(
        &self,
        id: &str,
        subject: &dyn Holder,
        start: Birthday,
    ) -> Result<Option<Watch>, IndexError> {
        self.validate(subject)?;
        let restated =
            |watch: &Watch| !matches!(start, Birthday::At(position) if position != watch.start);
        let held = self.with(|set| set.identified(id))?;
        if held.iter().any(|watch| watch.address != *subject.address()) {
            return Err(Self::conflict("this wallet id is another address's"));
        }
        if let Some(watch) = held
            .into_iter()
            .find(|watch| watch.asset == *subject.asset())
        {
            return if restated(&watch) {
                Ok(Some(watch))
            } else {
                Err(Self::conflict("this watch is already registered"))
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
            (Birthday::Fresh, None) => Err(IndexError::new(
                IndexErrorKind::Store,
                "a fresh watch requires a checkpoint or verified source tip",
                false,
            )),
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
    /// the guard — a borrow of the [`Rows`] does not outlive the call — so no guard
    /// is ever held across an await and no pass observes the set half-published.
    /// These two are the *only* access to `self.set`, which puts that property in
    /// their signatures instead of in the code that calls them: there is no way
    /// here to obtain a guard, so there is nothing for an `.await` to be written
    /// after.
    fn with<T>(&self, read: impl FnOnce(&Rows) -> T) -> Result<T, IndexError> {
        let set = self.set.read().map_err(|_| Self::poisoned())?;
        Ok(read(&set))
    }

    /// Changes the hot set under the lock, under [`Self::with`]'s invariant.
    fn with_mut<T>(&self, change: impl FnOnce(&mut Rows) -> T) -> Result<T, IndexError> {
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
/// appear or disappear.
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
/// [`Rows::restore`] inserts whichever of those rows the store actually holds,
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
    rows: Vec<WatchKey>,
}

impl Reconciliation<'_> {
    /// Names one row this call is about to write durably.
    fn writing(&mut self, key: WatchKey) {
        self.rows.push(key);
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
            for key in &self.rows {
                set.detach(key);
            }
        }));
        self.watches.mark_stale();
    }
}
