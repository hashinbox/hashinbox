//! One network's indexer: what an application calls on it, and everything a
//! pass advances, held in one place.
//!
//! [`Indexing`] and [`Checkpoint`] are the contract, and [`Indexer`] is the one
//! implementation every chain in this workspace returns. The state is here, and
//! the procedure that advances it is beside this file
//! ([`pass`](crate::pass), with [`catchup`](crate::catchup) and
//! [`backfill`](crate::backfill) as its two phases). One owner, and nothing
//! passes the watch set or the pending blocks to anything else.
//!
//! **No query is answered here.** A balance, a history page and a spendable set
//! are a repository's own reads. What an indexer adds for a consumer of them is
//! the two facts only an index holds: where its confirmed line is
//! ([`Checkpoint`]), and the blocks it holds above that line ([`Pending`]).

use std::sync::{Arc, Mutex, atomic::AtomicBool};

use crate::{
    Backfilling, Birthday, BlockInterpreter, BlockRef, BlockSource, BoxFuture, IndexError,
    IndexStore, Listener, Network, Pending, PendingSample, Repository, SyncConfig, SyncStatus,
    Watch, Watching, fetch::NativeBlocks, listeners::Listeners, pass::ConfirmedLine, pending,
    watches::Watches,
};

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
/// contract says so: a pass answers with the single [`SyncStatus`] of the single
/// network [`Self::network`] names. Many networks is what [`crate::Index`] is
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
/// the repository that answers it ([`crate::ConfirmedHistory`],
/// [`crate::BalanceStore`], [`crate::OutputStore`]) rather than reaching for it
/// through the thing doing the indexing. What an index adds to such an answer
/// is a sample of the blocks it holds above the line ([`crate::Pending`]), and
/// composing the two is the consumer's. There is no wider indexer trait above
/// this one; there was, carrying a balance read, and it was a courier — nothing
/// that held it read the balance it forwarded.
///
/// Implemented by [`crate::Indexer`], which is what each chain's own
/// constructor returns.
pub trait Indexing: Backfilling + Checkpoint + Watching {
    /// The one `(chain, network)` network this indexer synchronizes and answers
    /// for.
    fn network(&self) -> &Network;

    /// Notifies `listener` after each block this indexer commits. A list, not
    /// a slot; a composer forwards to every unit.
    fn listen(&self, listener: Arc<dyn Listener>);

    /// Advances this indexer's network, against its own watch list.
    ///
    /// **Bounded.** One pass does the work its [`crate::SyncPolicy`] allows and
    /// returns; it is not a loop, and nothing here schedules, retries or waits.
    /// A caller drives it — in its own loop, from a cron, or through the
    /// optional `crate::Supervisor`, which calls this and owns no second
    /// implementation of scanning or committing.
    ///
    /// The list is read during the pass rather than passed in, so it is never
    /// older than the tip the pass indexes towards: an address watched between
    /// the two reads has a birthday the pass's own blocks already cover.
    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>>;
}

/// One network's indexer, owning its watch set and pending blocks.
///
/// The source and interpreter preserve native chain semantics. The repository
/// requires only registration and atomic committing; optional queries are called
/// on the repository directly. One bounded pass runs at a time, while scheduling,
/// retry, quarantine and shutdown belong to the optional supervisor.
pub struct Indexer<S, I, R>
where
    I: BlockInterpreter,
{
    pub(crate) source: S,
    pub(crate) interpreter: I,
    pub(crate) repository: R,
    pub(crate) config: SyncConfig,
    /// What this network observes, and how far each observation's history has
    /// been read. Restored from the store once, then owned here.
    pub(crate) watches: Watches,
    /// The interpreted blocks above the confirmed line: what the pass will
    /// commit next. A consumer reads them as a sample ([`Pending`]) and never
    /// holds the collection.
    pub(crate) pending: pending::Blocks,
    /// The fetching path's own bounded cache of the native blocks behind the
    /// newest pending ones, so a re-inspection usually costs no refetch.
    pub(crate) native: NativeBlocks<I::Block>,
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
    pub fn new(source: S, interpreter: I, repository: R, config: SyncConfig) -> Self {
        let watches = Watches::new(config.network.clone(), config.policy());
        Self {
            source,
            interpreter,
            repository,
            config,
            watches,
            pending: pending::Blocks::default(),
            native: NativeBlocks::default(),
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

    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
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
    fn sampled(&self) -> Result<PendingSample, IndexError> {
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

    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
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

/// Cancelling is the index's own act on its own watch list, for the same reason
/// watching is: it writes the registration and the coverage that a pass reads.
impl<S, I, R> Backfilling for Indexer<S, I, R>
where
    S: Send + Sync,
    I: BlockInterpreter,
    R: Repository,
{
    fn cancel_backfill<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        Box::pin(async move {
            self.watches.owns(network)?;
            self.watches.load(&self.repository).await?;
            let persisted = self.repository.checkpoint(network).await?;
            self.watches
                .cancel_backfill(&self.repository, persisted)
                .await
        })
    }
}
