//! Every store port on ordinary maps, for every network in one process
//! (cargo feature `memory`).
//!
//! [`Repository`] is the second store this crate ships, and it is a store
//! rather than a test double: it answers the two required ports,
//! [`WatchStore`](crate::WatchStore) and [`IndexStore`](crate::IndexStore), and
//! every consumer capability — [`BalanceStore`](crate::BalanceStore),
//! [`OutputStore`](crate::OutputStore), [`Purging`](crate::Purging) and its own
//! [`ConfirmedHistory`](crate::ConfirmedHistory) — exactly as
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
//! here; the one commit is in `commits.rs` and the watch list in
//! `observations.rs`.
//!
//! Two collections reach the balance fold, not one. The history rows carry
//! every change a transaction made, and `credits` carries the changes no
//! transaction made — a consensus-layer withdrawal is value that arrived with
//! no transaction, no receipt and no trace entry (`docs/design/021` section
//! 4.1), so a fold that read only the first would be short by every one of
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

mod activity;
mod balances;
mod commits;
mod observations;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use crate::{
    Address, AssetId, Balance, BalanceQuery, BalanceStore, BlockCredit, BlockHeight, BlockRef,
    BoxFuture, CanonicalPage, CanonicalTransaction, Completeness, ConfirmedHistory, FeeSettlement,
    Fold, HistoryQuery, Holder, IndexError, IndexErrorKind, IndexedOutput, Inspection, MovementId,
    Network, OutputCursor, OutputId, OutputKey, OutputPage, OutputRequest, OutputStore,
    TransactionRef, Watch, storage::Probe,
};

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
    /// Which network holds each watch identity. Identities are unique across
    /// the whole store, exactly as SQLite's unique index on `id` spans the whole
    /// file.
    identities: BTreeMap<String, Network>,
    watches: BTreeMap<(Network, AssetId, String), Watch>,
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
type HistoryRows = BTreeMap<HistoryPlace, CanonicalTransaction>;

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
        transaction: &CanonicalTransaction,
    ) -> HistoryPlace {
        (
            network.clone(),
            address.value.clone(),
            transaction.block().height,
            transaction.transaction_order,
            transaction.transaction_id.clone(),
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

impl ConfirmedHistory for Repository {
    fn history<'a>(
        &'a self,
        query: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move {
            query.validate()?;
            self.read(|tables| Ok(Self::history_page(&query, tables)))
        })
    }
}

impl Repository {
    /// One page of `query`, selected out of the rows the address owns.
    fn history_page(query: &HistoryQuery, tables: &Tables) -> CanonicalPage {
        let checkpoint = tables.checkpoints.get(&query.address.network).cloned();
        let completeness = Self::provenance(query, tables, checkpoint.as_ref());
        let probe = Probe::new(query.limit);
        let read = Self::address_history(query, tables, probe.rows());
        let (transactions, next) = probe.page(read, |last| last.into());
        CanonicalPage {
            checkpoint,
            transactions,
            next,
            completeness,
        }
    }

    /// Up to `rows` matching transactions after the position, in order.
    fn address_history(
        query: &HistoryQuery,
        tables: &Tables,
        rows: usize,
    ) -> Vec<CanonicalTransaction> {
        let after = query.after.as_ref().map(|after| {
            (
                after.height,
                after.transaction_order,
                after.transaction.clone(),
            )
        });
        tables
            .history
            .iter()
            .filter(|((network, address, ..), _)| Self::owns(network, address, &query.address))
            .filter(|((.., height, order, id), _)| Self::after(after.as_ref(), *height, *order, id))
            .filter_map(|(_, transaction)| query.select(transaction.clone()))
            .take(rows)
            .collect()
    }

    /// Whether a row keyed by `(network, address)` belongs to this query.
    fn owns(network: &Network, address: &str, requested: &Address) -> bool {
        network == &requested.network && address == requested.value
    }

    /// Whether a history row is strictly after the cursor position.
    ///
    /// The three components compare in page order, so the ordinal decides
    /// between two transactions of one block and the identity only breaks a tie
    /// no chain produces.
    fn after(
        cursor: Option<&(BlockHeight, u64, TransactionRef)>,
        height: BlockHeight,
        order: u64,
        id: &TransactionRef,
    ) -> bool {
        cursor.is_none_or(|(at, ordinal, after)| (height, order, id) > (*at, *ordinal, after))
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
            revision: None,
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

impl BalanceStore for Repository {
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            query.validate()?;
            self.read(|tables| Self::settled(&query, tables))
        })
    }
}

impl Repository {
    /// The stored watched amount (or an unwatched fold), with one snapshot's provenance.
    fn settled(query: &BalanceQuery, tables: &Tables) -> Result<Balance, IndexError> {
        let key = (
            query.address.network.clone(),
            query.asset.clone(),
            query.address.value.clone(),
        );
        let net = match tables.balances.get(&key) {
            Some(net) => net.clone(),
            None => Self::recomputed(query, tables)?,
        };
        let ledger = Fold::from_net(query, net);
        let mut balance = ledger.balance(tables.checkpoints.get(&query.address.network).cloned());
        // Provenance only ever weakens the arithmetic's own answer: `Overdrawn`
        // is a proof the rows carry, and a claim about the range cannot talk it
        // away.
        if balance.completeness != Completeness::Overdrawn {
            balance.completeness = Self::provenance(query, tables, balance.observed_at.as_ref());
        }
        Ok(balance)
    }

    /// What the observations of `of`'s address can claim about the history
    /// behind the rows this store holds for it, at the `checkpoint` those rows
    /// are read at.
    ///
    /// The same rule the file-backed store applies, asked of the one owner both
    /// share ([`Inspection`]) because it is a property of the *watch rows*
    /// rather than of either backend.
    ///
    /// **Both reads ask it.** A balance and a history page project one
    /// address's rows two ways, so what is provable about the range behind them
    /// is one answer; a second copy of this rule for pages could disagree with
    /// this one, and nothing would say which had lied.
    fn provenance(of: &dyn Holder, tables: &Tables, checkpoint: Option<&BlockRef>) -> Completeness {
        let observations = Self::observing(tables, of.address());
        Inspection::new(&observations, checkpoint).completeness(of)
    }

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
