//! One network's indexer: everything a pass advances, held in one place.
//!
//! The state is here, the procedure that advances it is beside this file
//! ([`pass`](crate::pass), with [`catchup`](crate::catchup) and
//! [`backfill`](crate::backfill) as its two phases), and the reads over that
//! same state are in [`reads`](crate::reads). One owner, three files about it,
//! and nothing passes the watch set or the overlay to anything else.

use std::sync::{Arc, Mutex, atomic::AtomicBool};

use crate::{
    Backfilling, BlockInterpreter, BlockSource, BoxFuture, IndexError, Indexing, Listener, Network,
    Repository, Start, SyncConfig, SyncStatus, Watch, Watching, fetch::NativeBlocks,
    listeners::Listeners, overlay::Frontier, pass::ConfirmedLine, watches::Watches,
};

/// One chain's reusable indexing implementation: one network's watches, the
/// blocks it holds above the confirmed line, its chain adapter, and its
/// reference to the store.
///
/// Generic over a [`BlockSource`] `S`, a [`BlockInterpreter`] `I`, and a store
/// `R`. What it requires of `R` is [`Repository`] — [`crate::WatchStore`] plus
/// [`crate::IndexStore`] — and nothing else: a store that answers no balance, no
/// history page and no output still synchronizes, watches and commits through
/// it, which is what `tests/minimum_contract.rs` proves. Each consumer
/// capability `R` *does* offer is a further impl on this type, so a chain reads
/// exactly what its own store can answer.
///
/// One repository serves every network, so the same `R` is handed to each
/// network's service; a store that cannot be cloned is passed as `Arc<MySink>`.
///
/// **It owns the state its pass advances.** [`Indexing::sync`] is a bounded
/// procedure over these fields rather than a call into a second object holding
/// a second copy of them, which is what `docs/design/030 §8 (ex-032)` asked for: the watch
/// set is not a parameter, the overlay is not shared with a reader, and the
/// checkpoint a commit is compared against is read here. Only one pass runs at
/// a time; a concurrent call is a retryable `Conflict`. Scheduling, retries,
/// quarantine and shutdown are not here at all — they belong to the optional
/// `crate::Supervisor`, which calls [`Indexing::sync`] and owns no second
/// implementation of scanning or committing.
pub struct Service<S, I, R>
where
    I: BlockInterpreter,
{
    pub(crate) source: S,
    pub(crate) interpreter: I,
    pub(crate) repository: R,
    pub(crate) config: SyncConfig,
    /// What this network observes, and how far each observation's history has
    /// been read. Restored from the store once, then owned here.
    ///
    /// Behind an `Arc` for the same reason [`Self::pending`] is: a read names
    /// the inspection progress it gates on as one of its own dependencies
    /// ([`crate::ObservedHistory`]), rather than reaching into the service that
    /// advances it.
    pub(crate) watches: Arc<Watches>,
    /// The interpreted blocks above the confirmed line: what the pass will
    /// commit next, and what every read folds over what the store returned.
    ///
    /// Behind an `Arc` so a fixture can hold the same collection the service
    /// samples; nothing in production holds a second handle.
    pub(crate) pending: Arc<Frontier>,
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

impl<S, I, R> Service<S, I, R>
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
        Self::over(
            source,
            interpreter,
            repository,
            config,
            Arc::new(Frontier::default()),
        )
    }

    /// The same service over a `pending` collection the caller already holds.
    ///
    /// The seam the read-ordering fixtures need: what they pin is a commit
    /// landing between the overlay sample and the store read, so the store
    /// double has to reach the very collection the read samples.
    pub(crate) fn over(
        source: S,
        interpreter: I,
        repository: R,
        config: SyncConfig,
        pending: Arc<Frontier>,
    ) -> Self {
        let watches = Arc::new(Watches::new(config.network.clone(), config.policy()));
        Self {
            source,
            interpreter,
            repository,
            config,
            watches,
            pending,
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

impl<S, I, R> Indexing for Service<S, I, R>
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

/// The index owns what it observes: one durable row per observation, mirrored
/// by the hot set every pass reads.
impl<S, I, R> Watching for Service<S, I, R>
where
    S: Send + Sync,
    I: BlockInterpreter,
    R: Repository,
{
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn crate::Holder,
        start: Start,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        Box::pin(async move {
            self.watches.load(&self.repository).await?;
            let persisted = self.repository.checkpoint(self.network()).await?;
            self.watches
                .add(&self.repository, id, subject, start, persisted)
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
impl<S, I, R> Backfilling for Service<S, I, R>
where
    S: Send + Sync,
    I: BlockInterpreter,
    R: Repository,
{
    fn cancel<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        Box::pin(async move {
            self.watches.owns(network)?;
            self.watches.load(&self.repository).await?;
            let persisted = self.repository.checkpoint(network).await?;
            self.watches.cancel(&self.repository, persisted).await
        })
    }
}
