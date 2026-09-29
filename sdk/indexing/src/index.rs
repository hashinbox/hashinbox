//! What a caller holds: one index over one store and any number of chains.
//!
//! One chain or many, the caller holds the same [`Index`]: it answers for every
//! network its units own and advances each in turn, so adding a chain changes
//! composition and nothing else. Each unit is one network
//! ([`Indexing::network`]), so this is where many networks become one handle,
//! and the only place they do.
//!
//! [`Index::over`] is the assembly flow `docs/design/030 §8 (ex-032)` describes —
//! `Index::over(repository).chain(…)?.chain(…)?.build()?` — and it exists so a
//! caller hands the store to the *index* once instead of threading it through
//! each chain by hand. It launches nothing: what `build` does is validate, and
//! every pass afterwards is the caller's own bounded [`Index::sync`] or the
//! optional `crate::Supervisor`.
//!
//! The design's sketch spells that entry `Index::new`; it is `over` here because
//! a `new` that returns a builder rather than the type is `clippy`'s
//! `new_ret_no_self`, and this workspace carries no lint suppressions. The other
//! constructor, [`Index::of`], takes indexers that were composed some other way.

use std::{collections::BTreeSet, sync::Arc};

use crate::{
    Backfilling, BlockRef, BoxFuture, Checkpoint, Holder, IndexError, IndexErrorKind, Indexing,
    Listener, Network, Start, SyncPhase, SyncStatus, Watch, Watching,
};

/// One index over any number of independently implemented chains.
///
/// Reads and watches go to the unit that owns the requested network; [`Index::sync`]
/// advances each unit in order against that unit's own watch list. A single
/// chain is composed the same way as three: `Index::of(vec![indexer])`. A caller
/// that wants one chain's failure not to stop the others drives [`Index::units`]
/// instead.
///
/// **This is the only many-network value in the crate.** Every child is one
/// network ([`Indexing::network`]), so composing is what turns N networks into
/// one handle and nothing nests: an `Index` is not itself an [`Indexing`], and a
/// caller that held one as a child of another would have been asking for a unit
/// whose failures could not be isolated per network.
///
/// `C` is what the children answer, and it defaults to [`Indexing`] — the whole
/// of what one network's index is. **No query routes through here.** Watches,
/// checkpoints and `sync` do, because each of them is a statement about a
/// network and this is what knows which unit owns one; a balance and a page are
/// the repository's own answers, so a consumer serving several networks holds
/// one reader per network and asks the one that owns the subject
/// (`wallets::Composition`). A composition that routed a balance would have had
/// to demand the capability of every child before it could offer it for any.
pub struct Index<C: ?Sized = dyn Indexing> {
    units: Vec<SyncUnit<C>>,
    networks: Vec<Network>,
}

/// One store and the chains being assembled over it.
///
/// Built by [`Index::over`], finished by [`Assembly::build`]. Each
/// [`Assembly::chain`] answers `Result`, so the fluent form is
/// `Index::over(store).chain(…)?.chain(…)?.build()?` and a refusal is the
/// chain's own typed error at the chain that refused — no deferred failure state
/// and no chains constructed after one.
pub struct Assembly<R, C: ?Sized = dyn Indexing> {
    repository: R,
    chains: Vec<Arc<C>>,
}

impl<R: Clone, C: Indexing + ?Sized> Assembly<R, C> {
    /// Adds one chain, built over this index's store.
    ///
    /// `chain` is the chain crate's own constructor — its RPC client, its
    /// interpreter, its policy — handed the store and nothing else. It is
    /// synchronous and does no I/O: every chain in this workspace verifies its
    /// node *before* a store is opened (`docs/design/030 §8 (ex-028)` section 10.1), so by the
    /// time a store exists there is nothing left to await.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] carrying the chain's own message: at
    /// this point every failure is "this configuration cannot be assembled".
    pub fn chain<E: std::fmt::Display>(
        mut self,
        chain: impl FnOnce(R) -> Result<Arc<C>, E>,
    ) -> Result<Self, IndexError> {
        let indexer = chain(self.repository.clone()).map_err(|error| {
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                format!("a chain could not be composed: {error}"),
                false,
            )
        })?;
        self.chains.push(indexer);
        Ok(self)
    }

    /// The assembled index, or the first thing wrong with it.
    ///
    /// **Nothing is launched.** What this checks is that the chains' networks are
    /// disjoint; no task is spawned, no block is fetched and no store is
    /// written. Driving is the caller's, through [`Index::sync`] or the optional
    /// `crate::Supervisor`.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] when no chain was added, and
    /// [`IndexErrorKind::Conflict`] when two chains claim one network.
    pub fn build(self) -> Result<Index<C>, IndexError> {
        Index::of(self.chains)
    }
}

/// One child indexer and the network it owns: the unit of failure isolation.
///
/// A unit is independently drivable because each child reads only its own
/// watch list; nothing has to narrow a shared selection for it. One child is
/// one network, so a unit is one network too, and a driver keeps one status and
/// one health row per task.
pub struct SyncUnit<C: ?Sized = dyn Indexing> {
    indexer: Arc<C>,
}

impl<C: Indexing + ?Sized> Index<C> {
    /// Begins assembling an index over one store.
    ///
    /// The store is handed to each chain by [`Assembly::chain`] rather than by the
    /// caller, which is the whole of what this adds: one repository, every
    /// network, cloned once per chain. Nothing is validated and nothing is
    /// launched until [`Assembly::build`].
    #[must_use]
    pub fn over<R: Clone>(repository: R) -> Assembly<R, C> {
        Assembly {
            repository,
            chains: Vec::new(),
        }
    }

    /// Combines indexers whose networks are disjoint.
    ///
    /// The primitive [`Assembly::build`] ends at, and what a caller that composed
    /// its chains some other way — asynchronously, or through
    /// `wallets::Composition` — uses directly.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] for no indexers and
    /// [`IndexErrorKind::Conflict`] when two indexers claim one network.
    pub fn of(indexers: Vec<Arc<C>>) -> Result<Self, IndexError> {
        if indexers.is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "at least one indexer is required",
                false,
            ));
        }
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
            .map(|indexer| SyncUnit { indexer })
            .collect();
        Ok(Self { units, networks })
    }

    /// The independently drivable units, in construction order.
    #[must_use]
    pub fn units(&self) -> &[SyncUnit<C>] {
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

    /// Every unit's pass, then the answer — never "the units up to the first
    /// failure".
    ///
    /// **Bounded**, exactly as [`Indexing::sync`] is: one pass per unit, each
    /// doing the work its own [`crate::SyncPolicy`] allows, and no waiting,
    /// retrying or scheduling.
    ///
    /// **One chain's failure must not cost the others their pass.** That is the
    /// whole subject of `docs/design/019`, and it landed for the supervisor,
    /// which drives one [`SyncUnit`] per task. Bounded `sync` — the path an
    /// embedder without the `supervisor` feature drives, and the one
    /// `tests/minimum_contract.rs` and `examples/custom_store/` use — had lost
    /// it to a `?` inside this loop: a Bitcoin node refusing one call skipped
    /// Ethereum and Solana entirely, for as long as it kept refusing.
    ///
    /// # Errors
    ///
    /// The worst failure any unit reported: a terminal one outranks a
    /// retryable one, and the earliest of equal rank wins.
    pub fn sync<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SyncStatus>, IndexError>> {
        Box::pin(async move {
            let mut reported = Vec::with_capacity(self.units.len());
            for unit in &self.units {
                reported.push(unit.sync().await);
            }
            Self::outcome(reported)
        })
    }

    /// What a pass over every unit answers.
    ///
    /// A terminal failure outranks a retryable one, and the earliest of equal
    /// rank wins: a caller deciding whether to try again needs to hear about the
    /// network that will not recover by itself, not whichever one happens to be
    /// first. The error is passed through verbatim — kind, message and
    /// `retryable` — because that is what a driver and the supervisor's
    /// quarantine rule both read.
    ///
    /// The statuses of the units that *did* pass are not returned beside a
    /// failure, because one `Result` cannot carry both; what this guarantees is
    /// that their work happened. A caller that needs each network's outcome
    /// separately drives [`Index::units`], which is what the supervisor does.
    fn outcome(
        reported: Vec<Result<SyncStatus, IndexError>>,
    ) -> Result<Vec<SyncStatus>, IndexError> {
        let mut statuses = Vec::new();
        let mut failures = Vec::new();
        for outcome in reported {
            match outcome {
                Ok(passed) => statuses.push(passed),
                Err(failure) => failures.push(failure),
            }
        }
        match failures.into_iter().min_by_key(|failure| failure.retryable) {
            Some(failure) => Err(failure),
            None => Ok(statuses),
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

impl<C: Indexing + ?Sized> SyncUnit<C> {
    /// The one network this unit owns.
    #[must_use]
    pub fn network(&self) -> &Network {
        self.indexer.network()
    }

    /// Syncs this unit alone.
    ///
    /// # Errors
    ///
    /// Whatever the child reports, and [`IndexErrorKind::Store`] when the
    /// child's status names another network or claims [`SyncPhase::Ready`]
    /// without a persisted checkpoint.
    pub fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
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
    fn reported(network: &Network, status: SyncStatus) -> Result<SyncStatus, IndexError> {
        if status.network != *network
            || (status.phase == SyncPhase::Ready && status.checkpoint.is_none())
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

impl<C: Indexing + ?Sized> Checkpoint for Index<C> {
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
impl<C: Indexing + ?Sized> Watching for Index<C> {
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn Holder,
        start: Start,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        match self.indexer(&subject.address().network) {
            Ok(indexer) => indexer.watch(id, subject, start),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    /// An id is unique across the whole composition, so an unwatch asks each
    /// unit in turn and stops at the one that held it. Nothing else can find it:
    /// the caller has an id and not a network, which is exactly how
    /// `apps/api` addresses a wallet.
    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
        Box::pin(async move {
            for unit in &self.units {
                if let Some(watch) = unit.indexer.unwatch(id).await? {
                    return Ok(Some(watch));
                }
            }
            Ok(None)
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

/// A cancellation routes on its own network, exactly as a read does: one
/// network's owed range is nothing to do with another's.
impl<C: Indexing + ?Sized> Backfilling for Index<C> {
    fn cancel<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        match self.indexer(network) {
            Ok(indexer) => indexer.cancel(network),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}
