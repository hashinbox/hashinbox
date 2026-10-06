//! A caller-owned collection of network indexers over one shared repository.
//! Each unit keeps its own network state and failure; composition selects concrete
//! chains before the collection is driven.

use std::{collections::BTreeSet, sync::Arc};

use crate::{
    Backfilling, Birthday, BlockRef, BoxFuture, Checkpoint, Holder, IndexError, IndexErrorKind,
    Indexing, Listener, Network, SyncPhase, SyncStatus, Watch, Watching,
};

/// One index over any number of independently implemented chains.
///
/// Reads and watches go to the unit that owns the requested network; each of
/// [`Index::units`] is driven alone, so one chain's failure does not stop the
/// others. A single chain is composed the same way as three:
/// `Index::of(vec![indexer])`.
///
/// **This is the only many-network value in the crate.** Every child is one
/// network ([`Indexing::network`]), so composing is what turns N networks into
/// one handle and nothing nests: an `Index` is not itself an [`Indexing`], and a
/// caller that held one as a child of another would have been asking for a unit
/// whose failures could not be isolated per network.
///
/// `C` is what the children answer, and it defaults to [`Indexing`] — the whole
/// of what one network's index is. **No query routes through here.** Watches,
/// checkpoints and watches do, because each of them is a statement about a
/// network and this is what knows which unit owns one; a balance and a page are
/// the repository's own answers, so a consumer serving several networks holds
/// one reader per network and asks the one that owns the subject
/// (`sdk_config::Composition`). A composition that routed a balance would have had
/// to demand the capability of every child before it could offer it for any.
pub struct Index<C: ?Sized = dyn Indexing> {
    units: Vec<SyncUnit<C>>,
    networks: Vec<Network>,
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
    fn cancel_backfill<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        match self.indexer(network) {
            Ok(indexer) => indexer.cancel_backfill(network),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}
