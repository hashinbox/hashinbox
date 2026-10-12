//! Every store port on ordinary maps, for every network in one process
//! (cargo feature `memory`).
//!
//! [`Repository`] is the second store this crate ships, and it is a store
//! rather than a test double: it answers the two required ports,
//! [`WatchStore`](crate::WatchStore) and [`IndexStore`](crate::IndexStore), and
//! the narrow port [`OutputStore`](crate::OutputStore) — and the purge exactly
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
//! The tables, the checkpoint read, history and outputs are here; the one
//! commit is in `commits.rs`.
//!
//! Balances are running totals, as SQLite's `ledger` keeps them: one
//! `Movement` list per holding, in chain order, its last element's total
//! the balance. A list is only ever appended to; a change that would land at
//! or before its tail deletes the list from that block on, and the batch's
//! changes are appended again in order (`schema/sdk.sql`).
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

use crate::storage::Probe;
use crate::{
    Address, AssetId, BlockHeight, BlockPosition, BlockRef, BoxFuture, IndexError, IndexErrorKind,
    IndexedOutput, Inspection, Network, OutputCursor, OutputId, OutputKey, OutputPage,
    OutputRequest, OutputStore, Transaction, TransactionRef, Watch, WatchProgress, WatchStore,
};
use num_bigint::BigInt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

mod commits;
mod reads;

/// Every stored row, keyed by the domain values themselves.
#[derive(Clone, Default)]
struct Tables {
    /// Every block a fact or a claim names, by its network and native
    /// position: one block per position, so a second hash there is refused.
    blocks: BTreeMap<(Network, BlockPosition), BlockRef>,
    checkpoints: BTreeMap<Network, BlockRef>,
    /// Keyed `(network, address, height, transaction order, transaction id)`,
    /// which is the history page order: the ordinal sits between the height and
    /// the identity so an ordinary `BTreeMap` walk is the native block order
    /// `sqlite`'s keyset page reads, rather than a sort this store would have to
    /// perform.
    history: HistoryRows,
    outputs: BTreeMap<(Network, String, OutputId), IndexedOutput>,
    /// Every outpoint a required spend has named, matched or not, so no
    /// historical write can put one of them back.
    ///
    /// The `spent_block_id` of the file-backed store's `output` row, as a
    /// map-backed store spells it: there the creation and the spending are two
    /// halves of one row and a spend sets the reference rather than deleting
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
    /// unwatched outpoint spent in every block would grow with the chain. A
    /// tracked spend of a held output is here, as SQLite keeps that row spent.
    ///
    /// The value is the creation's asset, `None` for a spend-first tombstone
    /// (SQLite's `asset IS NULL`), so unwatch deletes by `(address, asset)`.
    spent: BTreeMap<(Network, String, OutputId), Option<AssetId>>,
    /// Every observation, by wallet id and asset: SQLite's `wallet_index`
    /// rows — the birthday, how far it has been inspected, and the persisted
    /// total.
    cursors: BTreeMap<(String, AssetId), Cursor>,
    /// Every holding's entries with their running totals, in chain order
    /// `(position, movement_order, id)`, exactly SQLite's `ledger` rows. The
    /// holding's address leads with its network.
    ledger: BTreeMap<(Address, AssetId), Vec<Movement>>,
}

/// One entry to a holding, and the running total after it: SQLite's
/// `ledger` row.
#[derive(Clone)]
struct Movement {
    entry: crate::storage::LedgerEntry,
    total: BigInt,
}

/// One observation as SQLite's `wallet_index` row holds it.
#[derive(Clone)]
struct Cursor {
    address: Address,
    birthday: BlockPosition,
    progress: Option<BlockRef>,
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

// The in-memory store's watch list: what it was told to observe, and how far
// each observation has been inspected. One `apply` for the whole batch, which
// is how a map-backed store expresses atomicity: the staged copy is installed
// only if every watch in the batch was accepted. It keeps no wallets: those are
// the wallets side's.

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
        Box::pin(async move { self.read(|tables| Ok(Self::paired(tables, network))) })
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
            // The observation and its indexed facts go together; other
            // watches' facts stay.
            self.apply(|tables| {
                let key = (watch.id.clone(), watch.asset.clone());
                let Some(cursor) = tables.cursors.get(&key) else {
                    return Ok(());
                };
                if cursor.address.network != *network {
                    return Ok(());
                }
                let address = cursor.address.clone();
                tables.cursors.remove(&key);
                let last = !tables.cursors.values().any(|held| held.address == address);
                tables.unindex(&address, &watch.asset, last);
                Ok(())
            })
        })
    }
}

impl Repository {
    /// Every observation of `network`, each beside the progress recorded for
    /// it, out of one snapshot.
    fn paired(tables: &Tables, network: &Network) -> Vec<WatchProgress> {
        tables
            .cursors
            .iter()
            .filter(|(_, cursor)| cursor.address.network == *network)
            .map(|((id, asset), cursor)| WatchProgress {
                watch: Watch {
                    id: id.clone(),
                    address: cursor.address.clone(),
                    asset: asset.clone(),
                    start: cursor.birthday,
                },
                indexed_through: cursor.progress.clone(),
            })
            .collect()
    }

    /// Every observation of `address`, whatever its asset, each beside the
    /// progress recorded for it, read under the same lock as the rows a query
    /// answers with ([`crate::Inspection`]).
    fn observing(tables: &Tables, address: &Address) -> Vec<WatchProgress> {
        Self::paired(tables, &address.network)
            .into_iter()
            .filter(|observed| observed.watch.address == *address)
            .collect()
    }

    /// The observation of `address` in `asset`, if any.
    fn cursor<'t>(tables: &'t Tables, address: &Address, asset: &AssetId) -> Option<&'t Cursor> {
        tables
            .cursors
            .iter()
            .find(|((_, held), cursor)| cursor.address == *address && held == asset)
            .map(|(_, cursor)| cursor)
    }

    /// Refuses a batch this store cannot write as one change: one network per
    /// change, and each row's own address and asset are [`Watch`]'s to answer
    /// for.
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

    /// Insert-if-absent, verify-if-present, for one watch of the batch: an
    /// exact repeat is accepted; the same wallet and asset otherwise, or the
    /// same `(address, asset)` under another id, is refused; a new
    /// observation starts with nothing inspected and deletes its holding's
    /// changes from its birthday on, which its own inspection appends again in
    /// order.
    fn register(tables: &mut Tables, watch: &Watch) -> Result<(), IndexError> {
        let key = (watch.id.clone(), watch.asset.clone());
        if let Some(cursor) = tables.cursors.get(&key) {
            return if cursor.address == watch.address && cursor.birthday == watch.start {
                Ok(())
            } else {
                Err(IndexError::conflict("address already registered"))
            };
        }
        if Self::cursor(tables, &watch.address, &watch.asset).is_some() {
            return Err(IndexError::conflict("address already registered"));
        }
        let holding = (watch.address.clone(), watch.asset.clone());
        if let Some(movements) = tables.ledger.get_mut(&holding) {
            movements.retain(|movement| movement.entry.position < watch.start);
        }
        tables.cursors.insert(
            key,
            Cursor {
                address: watch.address.clone(),
                birthday: watch.start,
                progress: None,
            },
        );
        Ok(())
    }
}
