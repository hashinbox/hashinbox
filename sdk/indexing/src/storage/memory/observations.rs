//! The in-memory store's watch list: what it was told to observe, and how far
//! each observation has been inspected.
//!
//! Separate from the parent module because the split falls where the ports do:
//! this is the only table an application writes and the index merely reads
//! back. Its rows are wallets, observed or archived, exactly as SQLite's
//! `wallet` rows are; what this port reads and writes are the observed ones,
//! and a withdrawal archives a row rather than removing it.
//!
//! One `apply` for the whole batch, which is how a map-backed store expresses
//! atomicity: the staged copy is installed only if every watch in the batch was
//! accepted, so a conflicting definition anywhere leaves no row behind.

use crate::{
    Address, AssetId, BoxFuture, IndexError, IndexErrorKind, Network, Watch, WatchProgress,
    WatchStore,
};

use super::{Repository, Tables, WalletRow};
use crate::storage::{wallet, watch::redefinition};

/// Where one wallet row is filed: `(network, asset, address)`.
pub(super) type WalletKey = (Network, AssetId, String);

impl WatchStore for Repository {
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            Self::one_network(&watches)?;
            self.apply(|tables| {
                watches
                    .iter()
                    .try_for_each(|watch| Self::register(tables, watch))
            })
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
                Self::archive(tables, network, &watch.id);
                Ok(())
            })
        })
    }

    fn cancel_backfill<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            Self::one_network(&watches)?;
            // One `apply`, so every re-anchor is staged together and installed
            // once: no reader ever sees the tables without the row, which is the
            // transaction this operation asks for.
            self.apply(|tables| {
                watches
                    .iter()
                    .try_for_each(|watch| Self::reanchor(tables, watch))
            })
        })
    }
}

impl Repository {
    /// One observation of the batch at the birthday it now carries, with the
    /// claim it earned under the old one gone.
    ///
    /// The preconditions are narrow on purpose: a re-anchor is a postcondition
    /// and repeating it is how a lost acknowledgement is settled, so a row the
    /// store does not hold — or holds archived — is observed as described. An
    /// identity held under another `(address, asset)` pair, in this network or
    /// another, is a different observation, which a re-anchor neither moves nor
    /// re-dates; a pair another identity holds is a name it does not take; and
    /// a birthday that goes *down* is refused for good, which is what stops a
    /// range that was never inspected from being claimed.
    fn reanchor(tables: &mut Tables, watch: &Watch) -> Result<(), IndexError> {
        let wallet_key = Self::wallet_key(watch);
        match Self::located(tables, &watch.id) {
            Some((held, _)) if held != wallet_key => Err(redefinition()),
            Some((_, row)) if row.archived_at.is_some() => {
                Self::observe(tables, &wallet_key, watch)
            }
            Some((_, row)) if row.birthday.is_some_and(|stored| watch.start < stored) => {
                Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "a re-anchor must not lower an observation's birthday",
                    false,
                ))
            }
            Some(_) => {
                if let Some(row) = tables.wallets.get_mut(&wallet_key) {
                    row.birthday = Some(watch.start);
                }
                drop(tables.progress.remove(&(wallet_key.0, watch.id.clone())));
                Ok(())
            }
            None => Self::register(tables, watch),
        }
    }

    /// Archives the observed row `network` holds under `id`, if any: its claim
    /// and total go; its birthday, label, key and every indexed fact stay.
    ///
    /// By identity, exactly as a SQL store's update is keyed: the `(asset,
    /// address)` pair a caller's watch names may be held by another identity,
    /// and that row is not this withdrawal's to archive.
    fn archive(tables: &mut Tables, network: &Network, id: &str) {
        let Some((wallet_key, row)) = Self::located(tables, id) else {
            return;
        };
        if wallet_key.0 != *network || row.archived_at.is_some() {
            return;
        }
        drop(tables.balances.remove(&wallet_key));
        drop(tables.progress.remove(&(network.clone(), id.to_owned())));
        if let Some(row) = tables.wallets.get_mut(&wallet_key) {
            row.archived_at = Some(wallet::now());
        }
    }

    /// The holding the row registered under `id` names, and the row, in
    /// whichever network holds it.
    ///
    /// Identities are unique across the whole store, so there is at most one.
    pub(super) fn located(tables: &Tables, id: &str) -> Option<(WalletKey, WalletRow)> {
        let network = tables.identities.get(id)?;
        tables
            .wallets
            .iter()
            .find(|((bound, ..), row)| bound == network && row.id == id)
            .map(|(wallet_key, row)| (wallet_key.clone(), row.clone()))
    }

    /// Every observation of `network`, each beside the progress recorded for it,
    /// out of one snapshot.
    fn paired(tables: &Tables, network: &Network) -> Vec<WatchProgress> {
        tables
            .wallets
            .iter()
            .filter(|((bound, ..), _)| bound == network)
            .filter_map(|(wallet_key, row)| row.watch(wallet_key))
            .map(|watch| WatchProgress {
                indexed_through: tables
                    .progress
                    .get(&(network.clone(), watch.id.clone()))
                    .cloned(),
                watch,
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
    /// both and a redefinition on one. An archived row of the same identity
    /// and holding is observed again, from the birthday given.
    fn register(tables: &mut Tables, watch: &Watch) -> Result<(), IndexError> {
        let wallet_key = Self::wallet_key(watch);
        match tables.wallets.get(&wallet_key) {
            Some(row) if row.watch(&wallet_key).as_ref() == Some(watch) => Ok(()),
            Some(row) if row.id == watch.id && row.archived_at.is_some() => {
                Self::observe(tables, &wallet_key, watch)
            }
            Some(_) => Err(redefinition()),
            None if tables.identities.contains_key(&watch.id) => Err(redefinition()),
            None => {
                tables
                    .identities
                    .insert(watch.id.clone(), wallet_key.0.clone());
                tables.wallets.insert(
                    wallet_key.clone(),
                    WalletRow {
                        id: watch.id.clone(),
                        birthday: None,
                        label: None,
                        secret: None,
                        archived_at: None,
                    },
                );
                Self::observe(tables, &wallet_key, watch)
            }
        }
    }

    /// Observes the row at `holding` from `watch`'s birthday, with nothing
    /// inspected for it and its total seeded from the rows already stored.
    fn observe(
        tables: &mut Tables,
        wallet_key: &WalletKey,
        watch: &Watch,
    ) -> Result<(), IndexError> {
        let query = crate::BalanceQuery {
            address: watch.address.clone(),
            asset: watch.asset.clone(),
        };
        let net = Self::recomputed(&query, tables)?;
        tables.balances.insert(wallet_key.clone(), net);
        drop(
            tables
                .progress
                .remove(&(wallet_key.0.clone(), watch.id.clone())),
        );
        if let Some(row) = tables.wallets.get_mut(wallet_key) {
            row.birthday = Some(watch.start);
            row.archived_at = None;
        }
        Ok(())
    }

    /// The holding `watch` names.
    fn wallet_key(watch: &Watch) -> WalletKey {
        (
            watch.address.network.clone(),
            watch.asset.clone(),
            watch.address.value.clone(),
        )
    }
}
