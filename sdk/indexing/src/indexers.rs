//! **Organ.**
//!
//! One network's indexer: what an application calls on it, and everything a
//! pass advances, held in one place.
//!
//! [`Indexing`] and [`Checkpoint`] are the contract, and [`Indexer`] is the one
//! implementation every chain in this workspace returns. The state is here, and
//! the procedure that advances it is beside this file
//! ([`pass`](crate::run), with [`catchup`](crate::run) and
//! [`backfill`](crate::run) as its two phases). One owner, and nothing
//! passes the watch set or the pending blocks to anything else.
//!
//! **No query is answered here.** A balance, a history page and a spendable set
//! are a repository's own reads. What an indexer adds for a consumer of them is
//! the two facts only an index holds: where its confirmed line is
//! ([`Checkpoint`]), and the blocks it holds above that line ([`Pending`]).

use crate::read::Cache;
use crate::run::ConfirmedLine;
use crate::watches::Watches;
use crate::{
    Birthday, BlockInterpreter, BlockRef, BlockSource, BoxFuture, Config, Holder, IndexError,
    IndexErrorKind, IndexStore, Network, Outcome, Pending, Phase, Repository, Sample, Watch,
    Watching, pending,
};
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

/// Reads how far a network is indexed.
pub trait Checkpoint: Send + Sync {
    /// The last committed block of `network`, or `None` before the first commit.
    ///
    /// This is the network's confirmed line, not its tip: blocks above it are
    /// held in memory and reported separately.
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;
}

/// What an index *is*: it observes **one** network, it advances it, and it says
/// where it got to.
///
/// **One network, one status.** Every indexer this workspace ships is one
/// chain's [`Indexer`](crate::Indexer) over one `(chain, network)`, so the
/// contract says so: a pass answers with the single [`Outcome`] of the single
/// network [`Self::network`] names. Many networks is what [`crate::Indexers`] is
/// for, and it aggregates these rather than being one of them — so nothing has
/// to check that a child reported each of its networks exactly once, and no
/// driver holds a vector of health rows per task.
///
/// **The whole of it, and the minimum.** Everything here is answerable by a
/// store that implements [`Repository`](crate::Repository) — registration and
/// committing — and nothing here reads a balance, a history page or an output.
/// That is the whole point of the split: a third party whose sink answers no
/// query at all still gets an index it can observe wallets with and drive
/// (`tests/minimum_contract.rs`), and a consumer that does read a query asks
/// the store that answers it ([`crate::OutputStore`]
/// or its own SQL) rather than reaching for it
/// through the thing doing the indexing. What an index adds to such an answer
/// is a sample of the blocks it holds above the line ([`crate::Pending`]), and
/// composing the two is the consumer's. There is no wider indexer trait above
/// this one; there was, carrying a balance read, and it was a courier — nothing
/// that held it read the balance it forwarded.
///
/// Implemented by [`crate::Indexer`], which is what each chain's own
/// constructor returns.
// linter:disable rust/trait-method-count -- Recovery resets the same owner after a purge.
pub trait Indexing: Checkpoint + Watching {
    /// The one `(chain, network)` network this indexer synchronizes and answers
    /// for.
    fn network(&self) -> &Network;

    /// Notifies `listener` after each block this indexer commits. A list, not
    /// a slot; a composer forwards to every unit.
    fn listen(&self, listener: Arc<dyn Listener>);

    /// The rhythm this indexer's chain is polled at, declared by the chain.
    /// A driver waits this long at the tip and bounds its retry wait by it.
    fn schedule(&self) -> crate::Schedule;

    /// Reloads this index after its network was purged ([`crate::IndexStore::purge`]),
    /// offline or quiesced online: pending blocks and caches are discarded and
    /// the observations reload without progress, so the next pass scans from
    /// their birthdays. The caller keeps this network's driver paused and its
    /// admissions blocked until this completes. Refused while the network still
    /// has a checkpoint; repeating it on a purged network is harmless.
    /// Existing wallet/send state remains alive. Unsupported implementations refuse.
    fn reset<'a>(&'a self) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async {
            Err(IndexError::new(
                crate::IndexErrorKind::InvalidRequest,
                "index does not support recovery",
                false,
            ))
        })
    }

    /// Advances this indexer's network, against its own watch list.
    ///
    /// **Bounded.** One pass does the work its [`crate::Budget`] allows and
    /// returns; it is not a loop, and nothing here schedules, retries or waits.
    /// A caller drives it — in its own loop, from a cron, or through the
    /// optional `crate::Driver`, which calls this and owns no second
    /// implementation of scanning or committing.
    ///
    /// The list is read during the pass rather than passed in, so it is never
    /// older than the tip the pass indexes towards: an address watched between
    /// the two reads has a birthday the pass's own blocks already cover.
    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<Outcome, IndexError>>;
}

/// One network's indexer, owning its watch set and pending blocks.
///
/// The source and interpreter preserve native chain semantics. The repository
/// requires only registration and atomic committing; optional queries are called
/// on the repository directly. One bounded pass runs at a time, while scheduling,
/// retry, quarantine and shutdown belong to the optional driver.
pub struct Indexer<S, I, R>
where
    I: BlockInterpreter,
{
    pub(crate) source: S,
    pub(crate) interpreter: I,
    pub(crate) repository: R,
    pub(crate) config: Config,
    /// What this network observes, and how far each observation's history has
    /// been read. Restored from the store once, then owned here.
    pub(crate) watches: Watches,
    /// The interpreted blocks above the confirmed line: what the pass will
    /// commit next. A consumer reads them as a sample ([`Pending`]) and never
    /// holds the collection.
    pub(crate) pending: pending::Blocks,
    /// The fetching path's own bounded cache of the native blocks behind the
    /// newest pending ones, so a re-inspection usually costs no refetch.
    pub(crate) native: Cache<I::Block>,
    /// Whether a pass is running: the one at a time this type allows.
    pub(crate) running: AtomicBool,
    /// The highest confirmed line this service has been told about, which is
    /// the whole of what the backwards guard needs.
    pub(crate) confirmed: Mutex<Option<ConfirmedLine>>,
    pub(crate) listeners: Listeners,
}

impl<S, I, R> Indexer<S, I, R>
where
    I: BlockInterpreter,
{
    /// Assembles a service. No I/O happens until the first `sync`.
    ///
    /// The repository is the durable half of everything: the watch list it
    /// reads back, and the two writes a pass makes. Nothing is cloned and no
    /// `Clone` is required of `R`, because there is one owner of it.
    #[must_use]
    pub fn new(source: S, interpreter: I, repository: R, config: Config) -> Self {
        let watches = Watches::new(config.network.clone(), config.policy());
        Self {
            source,
            interpreter,
            repository,
            config,
            watches,
            pending: pending::Blocks::default(),
            native: Cache::default(),
            running: AtomicBool::new(false),
            confirmed: Mutex::new(None),
            listeners: Listeners::default(),
        }
    }

    /// The one `(chain, network)` network this service synchronizes.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.config.network
    }

    /// Adds `listener` to the list notified after each committed block.
    ///
    /// Inherent as well as on [`Indexing`], so a chain's constructor can install
    /// its own hook before the service satisfies the trait's source and store
    /// bounds; [`Indexing::listen`] delegates here.
    pub fn listen(&self, listener: Arc<dyn Listener>) {
        self.listeners.add(listener);
    }
}

impl<S, I, R> Indexing for Indexer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
    R: Repository,
{
    fn network(&self) -> &Network {
        Self::network(self)
    }

    fn listen(&self, listener: Arc<dyn Listener>) {
        Self::listen(self, listener);
    }

    fn schedule(&self) -> crate::Schedule {
        self.config.schedule()
    }

    fn reset<'a>(&'a self) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(self.reload())
    }

    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<Outcome, IndexError>> {
        Box::pin(async move { self.advance().await })
    }
}

/// The confirmed line is the store's own checkpoint for this network.
impl<S, I, R> Checkpoint for Indexer<S, I, R>
where
    S: Send + Sync,
    I: BlockInterpreter,
    R: IndexStore,
{
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.repository.checkpoint(network)
    }
}

/// The pending facts, and the whole of what is exposed about them: one sample
/// of the blocks this index holds above its confirmed line.
///
/// No bound on `R`, because a sample is read from memory and asks the store
/// nothing. The blocks in it are the very values the next commit hands the
/// store, since there is one collection of them
/// (`tests/service_contract.rs`).
impl<S, I, R> Pending for Indexer<S, I, R>
where
    S: Send + Sync,
    I: BlockInterpreter,
    R: Send + Sync,
{
    fn sampled(&self) -> Result<Sample, IndexError> {
        self.pending.sampled()
    }
}

/// The index owns what it observes: one durable row per observation, mirrored
/// by the hot set every pass reads.
impl<S, I, R> Watching for Indexer<S, I, R>
where
    S: BlockSource,
    I: BlockInterpreter,
    R: Repository,
{
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn crate::Holder,
        start: Birthday,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        Box::pin(async move {
            self.watches.load(&self.repository).await?;
            if let Some(existing) = self.watches.existing(id, subject, start)? {
                return Ok(existing);
            }
            let persisted = self.repository.checkpoint(self.network()).await?;
            let fallback = if start == Birthday::Fresh && persisted.is_none() {
                let tip = self.source.tip().await?;
                if self.source.canonical_at(tip.position).await?.as_ref() != Some(&tip) {
                    return Err(IndexError::disconnected(
                        "fresh watch tip is no longer canonical",
                    ));
                }
                Some(tip)
            } else {
                None
            };
            self.watches
                .add(
                    &self.repository,
                    id,
                    subject,
                    start,
                    persisted,
                    fallback.as_ref(),
                )
                .await
        })
    }

    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        Box::pin(async move {
            self.watches.load(&self.repository).await?;
            let persisted = self.repository.checkpoint(self.network()).await?;
            self.watches.remove(&self.repository, id, persisted).await
        })
    }

    fn watched<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        Box::pin(async move {
            self.watches.owns(network)?;
            self.watches.load(&self.repository).await?;
            self.watches.rows()
        })
    }
}

// A caller-owned collection of network indexers over one shared repository.
// Each unit keeps its own network state and failure; composition selects concrete
// chains before the collection is driven.

/// One index over any number of independently implemented chains.
///
/// Reads and watches go to the unit that owns the requested network; each of
/// [`Indexers::units`] is driven alone, so one chain's failure does not stop the
/// others. A single chain is composed the same way as three:
/// `Indexers::of(vec![indexer])`.
///
/// **This is the only many-network value in the crate.** Every child is one
/// network ([`Indexing::network`]), so composing is what turns N networks into
/// one handle and nothing nests: an `Indexers` is not itself an [`Indexing`], and a
/// caller that held one as a child of another would have been asking for a unit
/// whose failures could not be isolated per network.
///
/// `C` is what the children answer, and it defaults to [`Indexing`] — the whole
/// of what one network's index is. **No query routes through here.** Watches,
/// checkpoints and watches do, because each of them is a statement about a
/// network and this is what knows which unit owns one; a balance and a page are
/// the repository's own answers, so a consumer serving several networks holds
/// one reader per network and asks the one that owns the subject
/// (`payments::config::Composition`). A composition that routed a balance would have had
/// to demand the capability of every child before it could offer it for any.
pub struct Indexers<C: ?Sized = dyn Indexing> {
    units: Vec<Route<C>>,
    networks: Vec<Network>,
}

/// One child indexer and the network it owns: the unit of failure isolation.
///
/// A unit is independently drivable because each child reads only its own
/// watch list; nothing has to narrow a shared selection for it. One child is
/// one network, so a unit is one network too, and a driver keeps one status and
/// one health row per task.
pub struct Route<C: ?Sized = dyn Indexing> {
    indexer: Arc<C>,
}

impl<C: Indexing + ?Sized> Indexers<C> {
    /// Combines indexers whose networks are disjoint. An empty collection is valid
    /// while a caller awaits configuration; it does not establish readiness.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Conflict`] when two indexers claim one network.
    pub fn of(indexers: Vec<Arc<C>>) -> Result<Self, IndexError> {
        let mut unique = BTreeSet::new();
        let mut networks = Vec::with_capacity(indexers.len());
        for indexer in &indexers {
            let network = indexer.network();
            if !unique.insert(network.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "indexers contain the same network",
                    false,
                ));
            }
            networks.push(network.clone());
        }
        let units = indexers
            .into_iter()
            .map(|indexer| Route { indexer })
            .collect();
        Ok(Self { units, networks })
    }

    /// The independently drivable units, in construction order.
    #[must_use]
    pub fn units(&self) -> &[Route<C>] {
        &self.units
    }

    /// Every `(chain, network)` network this index answers for, in composition
    /// order — one per unit.
    #[must_use]
    pub fn networks(&self) -> &[Network] {
        &self.networks
    }

    /// Notifies `listener` after each block any unit commits.
    pub fn listen(&self, listener: Arc<dyn Listener>) {
        for unit in &self.units {
            unit.indexer.listen(listener.clone());
        }
    }

    fn indexer(&self, network: &Network) -> Result<&C, IndexError> {
        self.units
            .iter()
            .find(|unit| unit.network() == network)
            .map(|unit| unit.indexer.as_ref())
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "index network is not configured",
                    false,
                )
            })
    }
}

impl<C: Indexing + ?Sized> Route<C> {
    /// The one network this unit owns.
    #[must_use]
    /// Shared owner, for caller-coordinated offline recovery and composition.
    pub fn indexer(&self) -> Arc<C> {
        self.indexer.clone()
    }

    /// The exact network owned by this unit.
    pub fn network(&self) -> &Network {
        self.indexer.network()
    }

    /// Syncs this unit alone.
    ///
    /// # Errors
    ///
    /// Whatever the child reports, and [`IndexErrorKind::Store`] when the
    /// child's status names another network or claims [`Phase::Ready`]
    /// without a persisted checkpoint.
    pub fn sync<'a>(&'a self) -> BoxFuture<'a, Result<Outcome, IndexError>> {
        Box::pin(async move {
            let reported = self.indexer.sync().await?;
            Self::reported(self.network(), reported)
        })
    }

    /// Accepts one child's pass only when the status is about the child's own
    /// network and never claims `Ready` without a persisted checkpoint.
    ///
    /// The invariant belongs to the [`Indexing`] implementation, so it is
    /// checked where the composition meets the child rather than in whatever
    /// loop happens to drive it. Every indexer this workspace ships satisfies
    /// it structurally; a third-party child is the case this catches, and it
    /// quarantines only that child.
    fn reported(network: &Network, status: Outcome) -> Result<Outcome, IndexError> {
        if status.network != *network
            || (status.phase == Phase::Ready && status.checkpoint.is_none())
        {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "indexer returned incomplete or inconsistent synchronization status",
                false,
            ));
        }
        Ok(status)
    }
}

impl<C: Indexing + ?Sized> Checkpoint for Indexers<C> {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        match self.indexer(network) {
            Ok(indexer) => indexer.checkpoint(network),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

/// Watches route on the subject's own network, exactly as reads do.
impl<C: Indexing + ?Sized> Watching for Indexers<C> {
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn Holder,
        start: Birthday,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        match self.indexer(&subject.address().network) {
            Ok(indexer) => indexer.watch(id, subject, start),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    /// An id is unique across the whole composition, so an unwatch asks each
    /// unit in turn and stops at the one that held it. Nothing else can find it:
    /// the caller has an id and not a network, which is exactly how
    /// `apps/server` addresses a wallet.
    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        Box::pin(async move {
            for unit in &self.units {
                let watches = unit.indexer.unwatch(id).await?;
                if !watches.is_empty() {
                    return Ok(watches);
                }
            }
            Ok(Vec::new())
        })
    }

    fn watched<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        match self.indexer(network) {
            Ok(indexer) => indexer.watched(network),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

// The post-commit hook: what one committed block changed, who is told about
// it, and the list one service tells.
//
// A list rather than a slot, and synchronous rather than awaited: a hook that
// cannot await cannot slow a pass by awaiting, and the dispatch keeps its
// allocation-free shape by sharing the list rather than copying it.

/// What one committed block changed, delivered after it was durably stored.
///
/// Only blocks that actually changed the index are reported: a block the store's
/// own line already covers is never presented again, so a restart that finds its
/// last commit landed does not re-notify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockCommit {
    /// The network that committed the block.
    pub network: Network,
    /// The committed block; now the network's checkpoint.
    pub block: BlockRef,
}

/// Notified after each block an index commits.
///
/// Registered with [`Indexing::listen`](crate::Indexing::listen), called once per
/// committed block in block order, on the synchronizing task, holding no lock
/// the caller can reach. It returns nothing and cannot fail: the block is
/// already durable, so there is nothing to refuse and nothing to report. Do no
/// I/O here — send to a channel. A veto belongs in a
/// [`IndexStore`](crate::IndexStore) implementation, which is the only thing
/// that runs inside the commit.
///
/// Delivery is **at most once, and never retracted**. A listener that must not
/// miss a block reads [`Checkpoint`](crate::Checkpoint) when it starts and
/// catches up through the store's own rows; this is a wake-up, not a
/// log.
pub trait Listener: Send + Sync {
    /// Called once per committed block, after the store returned.
    fn committed(&self, commit: &BlockCommit);
}

/// Every [`Listener`] registered on one service, in registration order.
///
/// The list is shared rather than owned by the lock: registration replaces it,
/// and a dispatch takes a share of whatever it was at that moment. That is what
/// lets [`Self::announce`] release the lock before it calls anything, which is
/// the whole of why a callback may re-enter [`Listeners::add`].
#[derive(Default)]
pub(crate) struct Listeners(RwLock<Arc<[Arc<dyn Listener>]>>);

impl Listeners {
    /// Adds `listener` to the end of the list.
    ///
    /// Copies the list to extend it, which costs one allocation per
    /// registration — a composition-time call, and the price of a dispatch that
    /// allocates nothing per block and holds nothing across a callback.
    ///
    /// A poisoned list is dropped rather than propagated: registration is a
    /// composition-time call with no error channel, and losing a notification
    /// never loses a durable fact.
    pub(crate) fn add(&self, listener: Arc<dyn Listener>) {
        if let Ok(mut listeners) = self.0.write() {
            let mut extended = listeners.to_vec();
            extended.push(listener);
            *listeners = extended.into();
        }
    }

    /// Tells every listener about one committed block, oldest first, after the
    /// store returned. It cannot fail, so nothing here can abort a pass.
    ///
    /// **The lock is released before the first callback runs.** A callback is a
    /// consumer's own code and may call anything, including
    /// [`Indexing::listen`](crate::Indexing::listen); holding the list across
    /// the loop turned that plain call into a deadlock of the whole pass, with
    /// no diagnostic. Taking a share of the list instead costs one refcount and
    /// nothing else.
    ///
    /// The list a block is announced to is the one registration left at the
    /// moment that block's announcement began, so a listener registered during
    /// a dispatch — by another thread, or by a callback in this very loop —
    /// joins at the next block. Delivery stays at most once and never
    /// retracted; a listener that must not miss a block still starts from its
    /// [`Checkpoint`](crate::Checkpoint), as [`Listener`] says.
    pub(crate) fn announce(&self, commit: &BlockCommit) {
        let Ok(listeners) = self.0.read().map(|listeners| Arc::clone(&listeners)) else {
            return;
        };
        for listener in listeners.iter() {
            listener.committed(commit);
        }
    }
}
