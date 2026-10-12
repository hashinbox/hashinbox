//! The in-memory store's write half: one commit, and the offline rescan.
//!
//! Separate from the parent module because that is the tables and the reads
//! over them; this is the one operation an index performs on them. What it
//! demonstrates is that [`crate::IndexStore::commit`] needs nothing a database
//! has — no conditional batch, no key framing, no versions — only that one
//! batch's history rows, output changes and progress claims become visible
//! together or not at all.
//!
//! **One rule for every block in the batch**: insert-if-absent, verify-if-
//! present, terminal on a row that disagrees. Whether the network's confirmed
//! line moves is decided by [`crate::IndexedBatch::checkpoint`] and changes
//! nothing about how a row is written, which is what collapsed two write paths
//! into this one.

use super::{Movement, Repository, Tables, conflict};
use crate::storage::LedgerEntry;
use crate::{
    Address, AssetId, BlockPosition, BlockRef, BoxFuture, IndexError, IndexErrorKind, IndexStore,
    IndexedBatch, Network, OutputChanges, OutputKey, Progress, PurgedRows,
};
use num_bigint::BigInt;
use std::collections::BTreeMap;

/// Keeps the rows `keep` accepts and counts the rows it dropped.
///
/// Every table of this store leads with the network that owns the row, so the
/// caller says which one it means by destructuring the key it already knows —
/// the same predicate [`BTreeMap::retain`] takes, which is the whole of the
/// removal.
fn dropped<K: Ord, V>(table: &mut BTreeMap<K, V>, mut keep: impl FnMut(&K) -> bool) -> u64 {
    let before = table.len();
    table.retain(|key, _| keep(key));
    u64::try_from(before - table.len()).unwrap_or(u64::MAX)
}

impl IndexStore for Repository {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.read(|tables| Ok(tables.checkpoints.get(network).cloned())) })
    }

    /// One batch, one change: the history rows, the folded output changes, every
    /// progress claim and the checkpoint land together or not at all, so a claim
    /// can never outlive the rows behind it.
    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move { self.apply(|tables| self.write(&batch, tables)) })
    }

    fn purge<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        Box::pin(async move {
            self.apply(|tables| {
                let checkpoint = u64::from(tables.checkpoints.remove(network).is_some());
                tables.blocks.retain(|(bound, _), _| bound != network);
                let history = dropped(&mut tables.history, |(bound, ..)| bound != network);
                let (credits, movements) = tables.entries_of(network);
                // SQLite counts every `output` row: live, spent and tombstone.
                let outputs = dropped(&mut tables.outputs, |(bound, ..)| bound != network)
                    .saturating_add(dropped(&mut tables.spent, |(bound, ..)| bound != network));
                tables
                    .ledger
                    .retain(|(address, _), _| address.network != *network);
                // Watch progress claims rows this purge just removed, so it
                // goes with them; the observations themselves are application
                // state and stay. Not counted: it is index state, not a row a
                // rescan re-creates.
                tables.disclaim(network);
                Ok(PurgedRows {
                    checkpoint,
                    history,
                    movements,
                    outputs,
                    credits,
                })
            })
        })
    }
}

impl Repository {
    /// The whole batch against one set of tables, inside the caller's change.
    fn write(
        &self,
        batch: &IndexedBatch,
        tables: &mut Tables,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let network = batch.network();
        if tables.checkpoints.get(network).as_ref() != batch.expected_checkpoint().as_ref() {
            return Err(conflict("checkpoint changed before commit"));
        }
        for canonical in batch.blocks() {
            Self::fill_block(network, canonical.block.reference(), tables)?;
        }
        for line in batch.progress().iter().map(|claim| &claim.indexed_through) {
            Self::fill_block(network, line, tables)?;
        }
        if let Some(line) = batch.checkpoint() {
            Self::fill_block(network, line, tables)?;
        }
        let mut unknown = Vec::new();
        // Block by block, creations before spends, in the order the chain built
        // them: an output created and then spent inside one batch leaves a
        // recorded spend rather than nothing at all, so replaying the block that
        // created it cannot offer it again.
        for canonical in batch.blocks() {
            self.fill_history(canonical.block.transactions(), tables)?;
            unknown.extend(self.fill_outputs(network, &canonical.outputs, tables)?);
        }
        let lost = Self::append(Self::changes(batch, tables)?, tables)?;
        for claim in batch.progress() {
            Self::advance(network, claim, tables)?;
        }
        // After the claims, so a watch whose own changes this batch rewrote
        // keeps the progress it just proved.
        Self::rewind(network, &lost, tables);
        if let Some(line) = batch.checkpoint() {
            drop(tables.checkpoints.insert(network.clone(), line.clone()));
        }
        Ok(unknown)
    }

    /// Records one claim, after comparing what it expected against what is held.
    ///
    /// The compare is the whole guard, and it is not a monotonic rule: a batch
    /// redone under a smaller budget legitimately settles less than the write it
    /// is redoing, so what is refused is a claim whose author was reading a
    /// different value — never one that went down.
    fn advance(network: &Network, claim: &Progress, tables: &mut Tables) -> Result<(), IndexError> {
        let key = (claim.watch_id.clone(), claim.asset.clone());
        let held = tables.cursors.get_mut(&key);
        let Some(cursor) = held.filter(|cursor| cursor.address.network == *network) else {
            return Err(conflict(
                "indexed progress names an observation this store does not hold",
            ));
        };
        if cursor.progress != claim.expected {
            return Err(conflict("recorded progress changed before this commit"));
        }
        cursor.progress = Some(claim.indexed_through.clone());
        Ok(())
    }

    /// Insert-if-absent, verify-if-present for one block a fact or claim
    /// names: one block per position, so another hash there is terminal.
    fn fill_block(
        network: &Network,
        block: &BlockRef,
        tables: &mut Tables,
    ) -> Result<(), IndexError> {
        let key = (network.clone(), block.position);
        match tables.blocks.get(&key) {
            Some(held) if held == block => Ok(()),
            Some(_) => Err(Self::corrupt(
                "a committed block disagrees with the stored one",
            )),
            None => {
                drop(tables.blocks.insert(key, block.clone()));
                Ok(())
            }
        }
    }

    /// Insert-if-absent, verify-if-present, for every address watched at the
    /// transaction's block: a row that differs is corruption rather than a
    /// duplicate. A counterparty nobody watches gets no row.
    fn fill_history(
        &self,
        transactions: &crate::Transactions<crate::Transaction>,
        tables: &mut Tables,
    ) -> Result<(), IndexError> {
        for transaction in transactions {
            let position = transaction.block().position;
            for address in transaction.addresses() {
                if !tables.takes_part(&address, position) {
                    continue;
                }
                self.fill_row(&address, transaction, tables)?;
            }
        }
        Ok(())
    }

    /// One address's row for one transaction, under the rule above.
    fn fill_row(
        &self,
        address: &Address,
        transaction: &crate::Transaction,
        tables: &mut Tables,
    ) -> Result<(), IndexError> {
        let key = Self::history_key(&transaction.id.network, address, transaction);
        match tables.history.get(&key) {
            Some(stored) if stored == transaction => Ok(()),
            Some(_) => Err(Self::corrupt(
                "a committed transaction disagrees with the stored one",
            )),
            None => {
                drop(tables.history.insert(key, transaction.clone()));
                Ok(())
            }
        }
    }

    /// The batch's folded output changes, with one cancellation the fold itself
    /// cannot make: an output some recorded spend already named is not written,
    /// because that spend was applied above where this batch reaches and no batch
    /// will ever carry it again. The record survives the cancellation, so a
    /// redone batch refuses the output exactly as the first run did.
    fn fill_outputs(
        &self,
        network: &Network,
        changes: &OutputChanges,
        tables: &mut Tables,
    ) -> Result<Vec<OutputKey>, IndexError> {
        for output in &changes.created {
            let key = Self::output_key(network, &output.key());
            if tables.outputs.get(&key).is_some_and(|held| held != output) {
                return Err(Self::corrupt(
                    "a committed output disagrees with the stored one",
                ));
            }
            // A creation whose spending is already known is filled in without
            // becoming spendable, which is the one thing discovering a creation
            // later must never do.
            if let Some(asset) = tables.spent.get_mut(&key) {
                // SQLite's upsert fills the tombstone's creation in, asset too.
                *asset = Some(output.asset.clone());
                continue;
            }
            drop(tables.outputs.insert(key, output.clone()));
        }
        let required = changes.spent.iter().map(|key| (key, true));
        let tracked = changes.tracked_spends.iter().map(|key| (key, false));
        let mut unknown = Vec::new();
        for (spent, required) in required.chain(tracked) {
            let key = Self::output_key(network, spent);
            let held = tables.outputs.remove(&key).map(|output| output.asset);
            if !required && held.is_none() {
                continue;
            }
            // The spend outlives the block that reported it whether or not it
            // matched: a later batch may still discover the creation below the
            // checkpoint, and no batch will ever carry this spend again.
            let matched = held.is_some();
            let recorded = tables.spent.entry(key).or_insert(None);
            if recorded.is_none() {
                *recorded = held;
            }
            if required && !matched {
                unknown.push(spent.clone());
            }
        }
        Ok(unknown)
    }

    /// A stored row that disagrees with the one being written: terminal, because
    /// retrying cannot repair it.
    pub(super) fn corrupt(message: &'static str) -> IndexError {
        IndexError::new(IndexErrorKind::Store, message, false)
    }
}

// Running totals: each holding's changes, appended in chain order.

impl Tables {
    /// Whether `address` is watched in `asset` at `position`: only such a
    /// holding gets changes and credits.
    pub(super) fn holds(
        &self,
        address: &Address,
        asset: &AssetId,
        position: BlockPosition,
    ) -> bool {
        self.cursors.iter().any(|((_, held), cursor)| {
            cursor.address == *address && held == asset && position >= cursor.birthday
        })
    }

    /// Whether `address` is watched in any asset at `position`: only such an
    /// address gets history rows.
    pub(super) fn takes_part(&self, address: &Address, position: BlockPosition) -> bool {
        self.cursors
            .values()
            .any(|cursor| cursor.address == *address && position >= cursor.birthday)
    }

    /// Deletes the facts indexed for `address` in `asset`: its entries and
    /// outputs (spent ones too), and — when `last`, no other asset of it
    /// watched — its history rows and spend-first tombstones.
    pub(super) fn unindex(&mut self, address: &Address, asset: &AssetId, last: bool) {
        let network = &address.network;
        let mine = |bound: &Network, held: &String| bound == network && *held == address.value;
        self.ledger.remove(&(address.clone(), asset.clone()));
        self.outputs
            .retain(|(bound, held, _), output| !mine(bound, held) || output.asset != *asset);
        self.spent.retain(|(bound, held, _), spent| {
            !mine(bound, held) || spent.as_ref().map_or(!last, |spent| spent != asset)
        });
        if last {
            self.history
                .retain(|(bound, held, ..), _| !mine(bound, held));
        }
    }

    /// How many of `network`'s entries are credits (no transaction) and
    /// movements: what a purge removes, counted as SQLite counts its rows.
    fn entries_of(&self, network: &Network) -> (u64, u64) {
        let entries = self
            .ledger
            .iter()
            .filter(|((address, _), _)| address.network == *network)
            .flat_map(|(_, movements)| movements);
        let (credits, movements): (Vec<_>, Vec<_>) =
            entries.partition(|movement| movement.entry.transaction.is_none());
        let count = |rows: Vec<_>| u64::try_from(rows.len()).unwrap_or(u64::MAX);
        (count(credits), count(movements))
    }

    /// A rescan preserves registrations but clears every claim about removed facts.
    pub(super) fn disclaim(&mut self, network: &crate::Network) {
        for cursor in self.cursors.values_mut() {
            if cursor.address.network == *network {
                cursor.progress = None;
            }
        }
    }
}

/// Where one entry sits in its holding's chain order, owned.
type Place = (BlockPosition, i64, String);

/// One batch's entries, by holding, then by place.
type Changes = BTreeMap<(Address, AssetId), BTreeMap<Place, LedgerEntry>>;

impl Movement {
    /// Where this movement sits in its holding's chain order.
    fn place(&self) -> (BlockPosition, i64, &str) {
        self.entry.place()
    }
}

impl Repository {
    /// Every entry one batch makes ([`LedgerEntry::of_block`]) to a holding
    /// watched at its block, never a counterparty's. One id cannot honestly
    /// carry two entries.
    fn changes(batch: &IndexedBatch, tables: &Tables) -> Result<Changes, IndexError> {
        let mut changes = Changes::new();
        for canonical in batch.blocks() {
            let entries = LedgerEntry::of_block(canonical, batch.settlement())?;
            let watched = entries
                .into_iter()
                .filter(|entry| tables.holds(&entry.address, &entry.asset, entry.position));
            for entry in watched {
                let holding = (entry.address.clone(), entry.asset.clone());
                let held = changes.entry(holding).or_default();
                let place = (entry.position, entry.movement_order, entry.id.clone());
                if held.get(&place).is_some_and(|other| *other != entry) {
                    return Err(Self::corrupt(
                        "a committed movement disagrees with the stored one",
                    ));
                }
                held.insert(place, entry);
            }
        }
        Ok(changes)
    }

    /// Appends one batch's entries to their holdings, forward-only, with
    /// their running totals, exactly as SQLite's `Ledger::insert_missing`;
    /// answers, per holding, the lowest position it lost: deleted from a
    /// rewound list and not carried by this batch.
    ///
    /// An entry already held must be the same entry. When a holding's
    /// first new entry sits at or before its tail, its list is cut from
    /// that block on and every entry of the batch from there is appended
    /// again, so a total is only ever its predecessor's plus its own change.
    fn append(
        changes: Changes,
        tables: &mut Tables,
    ) -> Result<BTreeMap<(Address, AssetId), BlockPosition>, IndexError> {
        let mut lost = BTreeMap::new();
        for (holding, batch) in changes {
            let movements = tables.ledger.entry(holding.clone()).or_default();
            let Some(first) = Self::first_fresh(movements, &batch)? else {
                continue;
            };
            let first_place = (first.0, first.1, first.2.as_str());
            let rewound = movements
                .last()
                .is_some_and(|tail| tail.place() >= first_place);
            let gone = rewound
                .then(|| Self::cut(movements, &batch, first.0))
                .flatten();
            if let Some(position) = gone {
                lost.insert(holding, position);
            }
            let mut total = movements
                .last()
                .map_or_else(BigInt::default, |last| last.total.clone());
            // Rewound: everything from the cut block; else: from the first
            // new entry on.
            let written = batch
                .into_iter()
                .filter(|(place, _)| (rewound && place.0 >= first.0) || *place >= first);
            for (_, entry) in written {
                total += entry.counted();
                movements.push(Movement {
                    entry,
                    total: total.clone(),
                });
            }
        }
        Ok(lost)
    }

    /// Removes `movements` from `position` on; the lowest position among the
    /// removed that `batch` does not carry again.
    fn cut(
        movements: &mut Vec<Movement>,
        batch: &BTreeMap<Place, LedgerEntry>,
        position: BlockPosition,
    ) -> Option<BlockPosition> {
        let cut = movements.partition_point(|movement| movement.entry.position < position);
        let carried =
            |movement: &Movement| batch.values().any(|entry| entry.id == movement.entry.id);
        movements
            .drain(cut..)
            .filter(|movement| !carried(movement))
            .map(|movement| movement.entry.position)
            .min()
    }

    /// The first place of `batch` `movements` does not hold, after checking
    /// that every entry it does hold is the same entry.
    fn first_fresh(
        movements: &[Movement],
        batch: &BTreeMap<Place, LedgerEntry>,
    ) -> Result<Option<Place>, IndexError> {
        let mut first = None;
        for (place, entry) in batch {
            let key = (place.0, place.1, place.2.as_str());
            let held = movements
                .binary_search_by(|movement| movement.place().cmp(&key))
                .ok()
                .and_then(|at| movements.get(at));
            let Some(held) = held else {
                first = first.or_else(|| Some(place.clone()));
                continue;
            };
            if held.entry != *entry {
                return Err(Self::corrupt(
                    "a committed movement disagrees with the stored one",
                ));
            }
        }
        Ok(first)
    }

    /// Moves the observation of a holding that lost changes it was inspected
    /// through back below them: to the highest stored block below the loss
    /// at or above its birthday, else to none.
    fn rewind(
        network: &Network,
        lost: &BTreeMap<(Address, AssetId), BlockPosition>,
        tables: &mut Tables,
    ) {
        for ((address, asset), position) in lost {
            let below = tables
                .blocks
                .range((network.clone(), BlockPosition(0))..(network.clone(), *position))
                .next_back()
                .map(|(_, block)| block.clone());
            let cursor = tables
                .cursors
                .iter_mut()
                .find(|((_, held), cursor)| cursor.address == *address && held == asset)
                .map(|(_, cursor)| cursor);
            let Some(cursor) = cursor else {
                continue;
            };
            if cursor
                .progress
                .as_ref()
                .is_none_or(|progress| progress.position < *position)
            {
                continue;
            }
            cursor.progress = below.filter(|block| block.position >= cursor.birthday);
        }
    }
}
