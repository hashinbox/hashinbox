//! Every store port on ordinary maps, for every network in one process
//! (cargo feature `memory`).
//!
//! [`Repository`] is the second store this crate ships, and it is a store
//! rather than a test double: it answers the two required ports,
//! [`WatchStore`](crate::WatchStore) and [`IndexStore`](crate::IndexStore), and
//! every narrow port — [`OutputStore`](crate::OutputStore),
//! [`TransactionStore`](crate::TransactionStore) — and the purge exactly
//! as
//! `sqlite::Repository` does (a code span, not a link: `sqlite` may be off),
//! and passes the same contract scenarios unmodified.
//!
//! **What it is for.** A process that indexes for as long as it runs and wants
//! nothing on disk: an embedding with no writable path, a short-lived worker, a
//! demonstration, and any test that would otherwise open a file and a temporary
//! directory. It needs no runtime and no C library, so a build that takes it
//! alone (`--no-default-features --features memory`) links neither.
//!
//! **What it is not.** Durable. Nothing survives the process that held it:
//! there is no file, no journal and no reopen, so a restart re-indexes from the
//! chain's first applicable block. It is also not the minimal sink a third
//! party has to write — that is two methods, and `examples/custom_store/` is
//! the reference for it.
//!
//! The tables, the checkpoint read, history, outputs and the balance fold are
//! here; the one commit is in `commits.rs`, the watch list in
//! `observations.rs` and the application's wallet registry in `wallets.rs`.
//!
//! Two collections reach the balance fold, not one. The history rows carry
//! every change a transaction made, and `credits` carries the changes no
//! transaction made — a consensus-layer withdrawal is value that arrived with
//! no transaction, no receipt and no trace entry, so a fold that read only the first would be
//! short by every one of
//! them and would say nothing about it.
//!
//! This store is deliberately unlike a file-backed one: ordinary maps keyed by
//! domain values, no physical keyspace, no compare-and-swap engine. What it
//! therefore proves, every time the shared contracts run against both, is that
//! the ports are expressible without either — a trait that quietly required one
//! backend's key framing or its conditional batch would fail here rather than a
//! year later.
//!
//! One store, every network, exactly as SQLite: each operation names the network
//! it is about and every key leads with it, so a read of one network cannot see
//! another's rows.
//!
//! Atomicity is expressed the only way a map-backed store can: a commit mutates
//! a copy and installs it, so a rejected change leaves every row as it was.

use crate::storage::watch::redefinition;
use crate::storage::{Probe, wallet};
use crate::{
    Address, AssetId, BlockCredit, BlockHeight, BlockRef, BoxFuture, FeeSettlement, IndexError,
    IndexErrorKind, IndexedOutput, Inspection, Location, MovementId, Network, OutputCursor,
    OutputId, OutputKey, OutputPage, OutputRequest, OutputStore, Registration, Transaction,
    TransactionRef, TransactionStore, Wallet, WalletRegistry, Watch, WatchProgress, WatchStore,
};
use crypto::SecretBytes;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

mod commits;
mod reads;

/// Every stored row, keyed by the domain values themselves.
#[derive(Clone, Default)]
struct Tables {
    checkpoints: BTreeMap<Network, BlockRef>,
    /// Keyed `(network, address, height, transaction order, transaction id)`,
    /// which is the history page order: the ordinal sits between the height and
    /// the identity so an ordinary `BTreeMap` walk is the native block order
    /// `sqlite`'s keyset page reads, rather than a sort this store would have to
    /// perform.
    history: HistoryRows,
    credits:
        BTreeMap<(Network, String, BlockHeight, MovementId), (crate::BlockPosition, BlockCredit)>,
    outputs: BTreeMap<(Network, String, OutputId), IndexedOutput>,
    /// Every outpoint a required spend has named, matched or not, so no
    /// historical write can put one of them back.
    ///
    /// The `spent` flag of the file-backed store's `output` row, as a
    /// map-backed store spells it: there the creation and the spending are two
    /// halves of one row and a spend sets the flag rather than deleting
    /// anything, so a replay of the creating block fills a creation in beside a
    /// spending that is already recorded. Keeping only the spends that *missed*
    /// would preserve the live set and lose exactly that — the block that
    /// created the output is below the checkpoint and a backfill will reach it,
    /// while the block that spent it is above the anchor and no chunk will ever
    /// carry it again.
    ///
    /// A **tracked** spend is not here, for the reason it is reported to nobody:
    /// no address claimed the outpoint, and the backfill of the address that
    /// eventually does claim it inspects this very spend again. One entry per
    /// unwatched outpoint spent in every block would grow with the chain.
    spent: BTreeSet<(Network, String, OutputId)>,
    /// Which network holds each wallet identity, observed or archived.
    /// Identities are unique across the whole store, exactly as SQLite's
    /// unique index on `id` spans the whole file.
    identities: BTreeMap<String, Network>,
    /// Every wallet, observed or archived, by the holding it names.
    wallets: BTreeMap<(Network, AssetId, String), WalletRow>,
    /// The persisted total of every *observed* holding; an archived one has
    /// none, and its reads fold the rows.
    balances: BTreeMap<(Network, AssetId, String), num_bigint::BigInt>,
    /// How far each observation has been inspected, by its own identity. One row
    /// per observation, not per address: a claim belongs to the watch that earned
    /// it under its own birthday.
    progress: BTreeMap<(Network, String), BlockRef>,
    /// Where each network's chain settles its network fee, as the batches that
    /// wrote these rows stated it. One entry per network, because it is a
    /// property of the chain rather than of a block: a store that keeps
    /// transactions rather than effects has to remember the rule the effects
    /// would have carried.
    settlements: BTreeMap<Network, FeeSettlement>,
}

/// One wallet as SQLite's `wallet` row holds it, less the holding that keys it.
///
/// The key is shared rather than copied when a change stages the tables, and
/// is zeroized when the last copy goes.
#[derive(Clone)]
struct WalletRow {
    id: String,
    /// `None` only for a wallet nothing ever observed.
    birthday: Option<crate::BlockPosition>,
    label: Option<String>,
    secret: Option<Arc<crypto::SecretBytes>>,
    /// `None` while observed.
    archived_at: Option<u64>,
}

impl WalletRow {
    /// The observation this row is, keyed by `(network, asset, address)`, or
    /// `None` while archived.
    fn watch(&self, (network, asset, address): &(Network, AssetId, String)) -> Option<Watch> {
        if self.archived_at.is_some() {
            return None;
        }
        Some(Watch {
            id: self.id.clone(),
            address: Address::new(network.clone(), address.clone()),
            asset: asset.clone(),
            start: self.birthday?,
        })
    }
}

/// Every store port, on maps held for as long as the process runs.
///
/// The analogue of one open database file, and clonable the way a handle to one
/// is: every clone is the *same* store, sharing one set of tables, so handing a
/// clone to an index and keeping one to read from is what this type is for.
/// Nothing here is durable — see this module's documentation.
#[derive(Clone, Default)]
pub struct Repository {
    tables: Arc<Mutex<Tables>>,
}

/// Where one transaction sits in one address's history:
/// `(network, address, height, transaction order, transaction id)`.
///
/// Named because the map is keyed by five values and a reader counting tuple
/// positions cannot tell the ordinal from the height.
type HistoryPlace = (Network, String, BlockHeight, u64, TransactionRef);

/// Every address's history rows, in page order.
type HistoryRows = BTreeMap<HistoryPlace, Transaction>;

impl Repository {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads under the lock. A poisoned lock is a terminal store failure.
    fn read<T>(
        &self,
        read: impl FnOnce(&Tables) -> Result<T, IndexError>,
    ) -> Result<T, IndexError> {
        let tables = self.tables.lock().map_err(|_| poisoned())?;
        read(&tables)
    }

    /// Applies `change` to a copy of the tables and installs it only if it
    /// succeeds, so a rejected change leaves every row as it was.
    ///
    /// Named for what it does to the tables rather than for the port method
    /// that calls it: [`IndexStore::commit`] is one caller of it, and so are the
    /// backfill, the purge and the watch list.
    fn apply<T>(
        &self,
        change: impl FnOnce(&mut Tables) -> Result<T, IndexError>,
    ) -> Result<T, IndexError> {
        let mut tables = self.tables.lock().map_err(|_| poisoned())?;
        let mut staged = tables.clone();
        let committed = change(&mut staged)?;
        *tables = staged;
        Ok(committed)
    }

    fn history_key(
        network: &Network,
        address: &Address,
        transaction: &Transaction,
    ) -> HistoryPlace {
        (
            network.clone(),
            address.value.clone(),
            transaction.block().height,
            transaction.order,
            transaction.id.clone(),
        )
    }

    fn output_key(network: &Network, key: &OutputKey) -> (Network, String, OutputId) {
        (
            network.clone(),
            key.address.value.clone(),
            key.output.clone(),
        )
    }
}

impl TransactionStore for Repository {
    fn locate<'a>(
        &'a self,
        transaction: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Location, IndexError>> {
        Box::pin(async move {
            self.read(|tables| {
                let block = tables
                    .history
                    .values()
                    .find(|held| &held.id == transaction)
                    .map(|held| held.block().clone());
                Ok(Location {
                    checkpoint: tables.checkpoints.get(&transaction.network).cloned(),
                    block,
                })
            })
        })
    }
}

impl Repository {
    /// Whether a row keyed by `(network, address)` belongs to this query.
    fn owns(network: &Network, address: &str, requested: &Address) -> bool {
        network == &requested.network && address == requested.value
    }
}

impl OutputStore for Repository {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            request.validate()?;
            self.read(|tables| Self::output_page(&request, tables))
        })
    }
}

impl Repository {
    /// One checkpoint-consistent page of `request`'s live outputs.
    fn output_page(request: &OutputRequest, tables: &Tables) -> Result<OutputPage, IndexError> {
        let checkpoint = tables.checkpoints.get(&request.network).cloned();
        // Refused under the lock the rows are read under, so the gate and the
        // rows it guards cannot be read at two different lines.
        let observations = Self::observing(tables, &request.address);
        Inspection::new(&observations, checkpoint.as_ref()).spendable()?;
        let cursor = request.after.as_ref();
        if cursor.is_some_and(|cursor| cursor.checkpoint != checkpoint) {
            return Err(conflict("outputs changed during pagination"));
        }
        let after = cursor.map(|cursor| cursor.position.clone());
        let probe = Probe::new(request.limit);
        let read = tables
            .outputs
            .iter()
            .filter(|((network, address, _), _)| Self::owns(network, address, &request.address))
            .filter(|((.., id), _)| after.as_ref().is_none_or(|after| &position(id) > after))
            .map(|(_, output)| output.clone())
            .take(probe.rows())
            .collect::<Vec<_>>();
        let (outputs, next) = probe.page(read, |output| OutputCursor {
            checkpoint: checkpoint.clone(),
            position: position(&output.id),
        });
        Ok(OutputPage {
            checkpoint,
            outputs,
            next,
        })
    }
}

/// The opaque resume position of one output, deliberately not a backend's key
/// framing: ordering an [`OutputId`] is all the port requires.
fn position(id: &OutputId) -> Vec<u8> {
    format!("{}\u{1}{:010}", id.transaction.id, id.index).into_bytes()
}

fn conflict(message: &str) -> IndexError {
    IndexError::new(IndexErrorKind::Conflict, message, true)
}

fn poisoned() -> IndexError {
    IndexError::new(
        IndexErrorKind::Store,
        "the in-memory store's lock is poisoned",
        false,
    )
}

impl Repository {
    /// Where one credit is filed: the address it credits, the height it was
    /// made at, and its own identity — the key both write paths share.
    fn credit_key(
        network: &Network,
        credit: &BlockCredit,
        height: BlockHeight,
    ) -> (Network, String, BlockHeight, MovementId) {
        (
            network.clone(),
            credit.to.value.clone(),
            height,
            credit.id.clone(),
        )
    }
}

// The in-memory store's watch list: what it was told to observe, and how far
// each observation has been inspected.
//
// Separate from the parent module because the split falls where the ports do:
// this is the only table an application writes and the index merely reads
// back. Its rows are wallets, observed or archived, exactly as SQLite's
// `wallet` rows are; what this port reads and writes are the observed ones,
// and a withdrawal archives a row rather than removing it.
//
// One `apply` for the whole batch, which is how a map-backed store expresses
// atomicity: the staged copy is installed only if every watch in the batch was
// accepted, so a conflicting definition anywhere leaves no row behind.

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
}

impl Repository {
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
    fn located(tables: &Tables, id: &str) -> Option<(WalletKey, WalletRow)> {
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
    fn observing(tables: &Tables, address: &Address) -> Vec<WatchProgress> {
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
        let subject = crate::Subject {
            address: watch.address.clone(),
            asset: watch.asset.clone(),
        };
        let net = Self::recomputed(&subject, tables)?;
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

// The in-memory store's wallet registry ([`WalletRegistry`]): filing a wallet
// with its label and key, and reading every row back, archived ones included.
// The rules are SQLite's, and the shared contract (`tests/contracts/wallets.rs`)
// holds both stores to them.

impl WalletRegistry for Repository {
    fn register<'a>(
        &'a self,
        registration: Registration,
    ) -> BoxFuture<'a, Result<Wallet, IndexError>> {
        Box::pin(async move {
            registration.validate()?;
            let (wallet_key, row) = self.apply(|tables| Self::file(tables, registration))?;
            Self::wallet_of(&wallet_key, &row)
        })
    }

    fn wallet<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Wallet>, IndexError>> {
        Box::pin(async move {
            self.read(|tables| Ok(Self::located(tables, id)))?
                .map(|(wallet_key, row)| Self::wallet_of(&wallet_key, &row))
                .transpose()
        })
    }

    fn wallets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Wallet>, IndexError>> {
        Box::pin(async move {
            let mut rows = self.read(|tables| {
                Ok(tables
                    .wallets
                    .iter()
                    .map(|(wallet_key, row)| (wallet_key.clone(), row.clone()))
                    .collect::<Vec<_>>())
            })?;
            rows.sort_by(|(_, left), (_, right)| left.id.cmp(&right.id));
            rows.iter()
                .map(|(wallet_key, row)| Self::wallet_of(wallet_key, row))
                .collect()
        })
    }
}

impl Repository {
    /// Files `registration` under the rules of [`WalletRegistry::register`],
    /// and returns the row as stored.
    fn file(
        tables: &mut Tables,
        registration: Registration,
    ) -> Result<(WalletKey, WalletRow), IndexError> {
        let wallet_key = (
            registration.address.network.clone(),
            registration.asset.clone(),
            registration.address.value.clone(),
        );
        let secret = registration.secret.map(Arc::new);
        let Some(row) = tables.wallets.get_mut(&wallet_key) else {
            if tables.identities.contains_key(&registration.id) {
                return Err(redefinition());
            }
            let row = WalletRow {
                id: registration.id,
                birthday: None,
                label: registration.label,
                secret,
                archived_at: Some(wallet::now()),
            };
            tables
                .identities
                .insert(row.id.clone(), wallet_key.0.clone());
            tables.wallets.insert(wallet_key.clone(), row.clone());
            return Ok((wallet_key, row));
        };
        if row.archived_at.is_none() && row.id != registration.id {
            return Err(redefinition());
        }
        match (&row.secret, secret) {
            (Some(held), Some(given)) if held.as_bytes() != given.as_bytes() => {
                return Err(wallet::another_key());
            }
            (None, given) => row.secret = given,
            _ => {}
        }
        if registration.label.is_some() {
            row.label = registration.label;
        }
        Ok((wallet_key, row.clone()))
    }

    /// The public wallet one row is, with its own copy of the key.
    fn wallet_of(
        (network, asset, address): &WalletKey,
        row: &WalletRow,
    ) -> Result<Wallet, IndexError> {
        let registration = Registration {
            id: row.id.clone(),
            address: Address::new(network.clone(), address.clone()),
            asset: asset.clone(),
            label: row.label.clone(),
            secret: row
                .secret
                .as_deref()
                .map(|secret| SecretBytes::new(secret.as_bytes())),
        };
        Wallet::new(registration, row.birthday, row.archived_at)
    }
}
