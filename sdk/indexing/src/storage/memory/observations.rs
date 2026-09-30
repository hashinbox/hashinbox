//! The in-memory store's watch list: what it was told to observe, and how far
//! each observation has been inspected.
//!
//! Separate from the parent module because the split falls where the ports do:
//! this is the only table an application writes and the index merely reads
//! back. Its rows are the observations themselves, which is what it is named
//! for — an address is one field of one.
//!
//! One `apply` for the whole batch, which is how a map-backed store expresses
//! atomicity: the staged copy is installed only if every watch in the batch was
//! accepted, so a conflicting definition anywhere leaves no row behind.

use crate::{
    Address, BoxFuture, Holder, IndexError, IndexErrorKind, Network, Watch, WatchProgress,
    WatchStore,
};

use super::{Repository, Tables, conflict};

impl WatchStore for Repository {
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            Self::one_network(&watches)?;
            self.apply(|tables| Self::register_all(tables, &watches))
        })
    }

    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        Box::pin(async move {
            // One `read`, so the observations and their progress are one
            // snapshot: the pairing below cannot be of a watch and another
            // watch's progress, whatever else is writing.
            self.read(|tables| Ok(Self::paired(tables, network)))
        })
    }

    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            if !watch.address.belongs_to(network) {
                return Err(IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "watch belongs to another network",
                    false,
                ));
            }
            self.apply(|tables| {
                Self::withdraw(tables, network, watch);
                Ok(())
            })
        })
    }

    fn cancel_backfill<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            Self::one_network(&watches)?;
            // One `apply`, so the withdrawal and the registration are staged
            // together and installed once: no reader ever sees the tables without
            // the row, which is the transaction this operation asks for.
            self.apply(|tables| Self::renewed(tables, &watches))
        })
    }
}

impl Repository {
    /// Every observation of the batch at the birthday it now carries, with the
    /// claim it earned under the old one gone.
    ///
    /// The withdrawal and the registration `forget` and `add` already own, in one
    /// staged copy rather than two installed ones — which is the whole of what the
    /// port asks for, so nothing new decides anything here.
    fn renewed(tables: &mut Tables, watches: &[Watch]) -> Result<(), IndexError> {
        for watch in watches {
            Self::raises(tables, watch)?;
        }
        for watch in watches {
            // By identity, exactly as a SQL store's delete is keyed: the row this
            // re-anchors is the one wearing that name. A `(asset, address)` pair
            // some *other* identity holds is a conflict the registration below
            // refuses, not a name a re-anchor takes.
            let held = tables
                .watches
                .values()
                .find(|held| held.id == watch.id)
                .cloned();
            if let Some(held) = held {
                Self::withdraw(tables, &held.address.network, &held);
            }
        }
        Self::register_all(tables, watches)
    }

    /// Refuses a re-anchor that would lower an observation's birthday.
    ///
    /// The one precondition the operation has, and it is narrow on purpose: a
    /// re-anchor is a postcondition and repeating it is how a lost
    /// acknowledgement is settled, so refusing a row the store does not hold
    /// would make that recovery impossible. Refusing a birthday that goes *down*
    /// costs none of it — raising to a birthday already stored is a no-op — and
    /// it is what stops a range that was never inspected from being claimed.
    fn raises(tables: &Tables, watch: &Watch) -> Result<(), IndexError> {
        let held = tables.watches.values().find(|held| held.id == watch.id);
        match held {
            Some(held) if watch.start < held.start => Err(conflict(
                "a re-anchor must not lower an observation's birthday",
            )),
            _ => Ok(()),
        }
    }

    /// Every watch of the batch, or none of them.
    fn register_all(tables: &mut Tables, watches: &[Watch]) -> Result<(), IndexError> {
        for watch in watches {
            Self::register(tables, watch)?;
        }
        Ok(())
    }

    /// Every observation of `network`, each beside the progress recorded for it,
    /// out of one snapshot.
    fn paired(tables: &Tables, network: &Network) -> Vec<WatchProgress> {
        tables
            .watches
            .iter()
            .filter(|((bound, ..), _)| bound == network)
            .map(|(_, watch)| WatchProgress {
                indexed_through: tables
                    .progress
                    .get(&(network.clone(), watch.id.clone()))
                    .cloned(),
                watch: watch.clone(),
            })
            .collect()
    }

    /// Every observation of `address`, whatever its asset, each beside the
    /// progress recorded for it.
    ///
    /// What a query of the address's rows is honest with
    /// ([`crate::Inspection`]). It is read under the same lock as
    /// those rows, so a claim about how far the address has been inspected is a
    /// claim about the very rows returned.
    pub(super) fn observing(tables: &Tables, address: &Address) -> Vec<WatchProgress> {
        Self::paired(tables, &address.network)
            .into_iter()
            .filter(|observed| observed.watch.address == *address)
            .collect()
    }

    /// Removes one observation, its identity and the progress it earned.
    ///
    /// Progress is per observation, so there is no shared row to orphan: another
    /// observation of the same address keeps its own claim, earned under its own
    /// birthday.
    fn withdraw(tables: &mut Tables, network: &Network, watch: &Watch) {
        let key = (
            network.clone(),
            watch.asset.clone(),
            watch.address.value.clone(),
        );
        if tables.watches.remove(&key).is_some() {
            drop(tables.identities.remove(&watch.id));
        }
        drop(tables.progress.remove(&(network.clone(), watch.id.clone())));
    }

    /// Refuses a batch this store cannot write as one change.
    ///
    /// One network is the only rule this store owns: the staged copy is
    /// installed once, so two networks would be one change over rows of both.
    /// Each row's own address and asset are [`Watch`]'s to answer for.
    fn one_network(watches: &[Watch]) -> Result<(), IndexError> {
        let Some(first) = watches.first() else {
            return Ok(());
        };
        for watch in watches {
            watch.validate()?;
            if watch.address.network != first.address.network {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "a batch of watches must all belong to one network",
                    false,
                ));
            }
        }
        Ok(())
    }

    /// Insert-if-absent, verify-if-present, for one watch of the batch.
    ///
    /// Both of the store's uniqueness constraints are consulted — the
    /// `(asset, address)` pair and the identity — because a repeat collides on
    /// both and a redefinition on one.
    fn register(tables: &mut Tables, watch: &Watch) -> Result<(), IndexError> {
        let network = watch.address.network.clone();
        let key = (
            network.clone(),
            watch.asset.clone(),
            watch.address.value.clone(),
        );
        let claimed = tables.identities.get(&watch.id);
        match (tables.watches.get(&key), claimed) {
            (Some(stored), _) if stored == watch => Ok(()),
            (Some(_), _) | (None, Some(_)) => Err(conflict(
                "an address is already registered for this identity or network",
            )),
            (None, None) => {
                tables.identities.insert(watch.id.clone(), network);
                tables.watches.insert(key, watch.clone());
                Ok(())
            }
        }
    }
}
