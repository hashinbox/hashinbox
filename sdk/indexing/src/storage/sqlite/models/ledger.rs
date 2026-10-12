//! `ledger`: one balance change, for one address, in one asset, and the
//! holding's running total after it.
//!
//! **A row is an effect, not a fact.** The fact is the transaction, stored once
//! in `"transaction"`; what is stored per address is what that transaction did
//! to its subject: a signed `diff`, whose sign is the direction, and a `kind`.
//! A network fee is one of these effects — a debit of the payer, naming its
//! transaction — and is the only place a fee is stored: history reads it back
//! from here.
//!
//! **Transactionless credits are these rows too.** A consensus-layer withdrawal
//! is one address, one asset, one amount and no transaction to name it, which
//! is this row with a null `transaction_id`. Its id carries an empty
//! transaction part, which no transaction has
//! (`Transaction::validate_identity`), so no credit can ever collide with a
//! movement.
//!
//! **The balance is the tail.** Rows of one holding are kept in chain order
//! (`block_position`, `movement_order`, `id`) and only ever appended: `total`
//! is the previous row's total plus this row's change. A row that would land at
//! or before the tail deletes the holding's rows from its block onward, and
//! they are written again in order (`schema/sdk.sql`).

use std::collections::{BTreeMap, BTreeSet};

use num_bigint::BigInt;
use sqlx::{FromRow, QueryBuilder, SqliteConnection, sqlite::SqliteRow};

use super::{
    Birthdays, Block, Chunks, address_id, canonical_id, column, fetched, native_part, raised,
    restored, store, stored,
};
use crate::storage::ledger::Effects as History;
use crate::storage::sqlite::found;
use crate::storage::{Kind, LedgerEntry};
use crate::{Address, AssetId, BlockPosition, Holder, IndexError, IndexedBatch, Network};

/// For each `(address id, asset)` holding a commit deleted rows of without
/// writing them again: the lowest block position it lost. A watch inspected
/// through there no longer has those rows and must inspect them again.
pub(in crate::storage::sqlite) type Lost = BTreeMap<(String, String), i64>;

/// What one transaction did to one address, as history shows it.
pub(in crate::storage::sqlite) type Effects = History;

/// One `ledger` row: one [`LedgerEntry`] as the file holds it, with its running
/// total.
///
/// Equality is the whole row; a re-presented entry is verified on every
/// column but `total`, which the store derives ([`Self::same_fact`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::storage::sqlite) struct Ledger {
    id: String,
    block_id: String,
    transaction_id: Option<String>,
    address_id: String,
    asset: String,
    movement_id: String,
    block_position: i64,
    movement_order: i64,
    effect: Kind,
    diff: String,
    total: String,
    counterparty: Option<String>,
}

impl Ledger {
    /// The row of `entry`, made in the block `block_id` names.
    fn from_entry(entry: LedgerEntry, block_id: String) -> Result<Self, IndexError> {
        let network = &entry.address.network;
        Ok(Self {
            id: entry.id,
            block_id,
            transaction_id: entry
                .transaction
                .map(|id| canonical_id(&[network.chain.as_str(), &network.name, &id])),
            address_id: address_id(network, &entry.address.value),
            asset: entry.asset.asset,
            movement_id: entry.movement_id,
            block_position: stored(entry.position.0)?,
            movement_order: entry.movement_order,
            effect: entry.kind,
            diff: entry.diff.to_string(),
            total: String::new(),
            counterparty: entry.counterparty,
        })
    }

    /// The entry this row holds, in `network`.
    fn entry(&self, network: &Network) -> Result<LedgerEntry, IndexError> {
        Ok(LedgerEntry {
            id: self.id.clone(),
            address: Address::new(network.clone(), native_part(&self.address_id)),
            asset: AssetId {
                chain: network.chain.clone(),
                asset: self.asset.clone(),
            },
            position: BlockPosition(restored(self.block_position)?),
            transaction: self.transaction_id.as_deref().map(native_part),
            movement_id: self.movement_id.clone(),
            movement_order: self.movement_order,
            kind: self.effect,
            diff: self.diff()?,
            counterparty: self.counterparty.clone(),
        })
    }

    /// The signed change.
    fn diff(&self) -> Result<BigInt, IndexError> {
        self.diff
            .parse()
            .map_err(|_| store("a stored change is not an atomic integer"))
    }

    /// What this row adds to its holding's total ([`LedgerEntry::counted`]).
    fn change(&self) -> Result<BigInt, IndexError> {
        if self.effect.counts(&self.movement_id) {
            return self.diff();
        }
        Ok(BigInt::ZERO)
    }

    /// The running total after this row.
    fn running(&self) -> Result<BigInt, IndexError> {
        self.total
            .parse()
            .map_err(|_| store("a stored total is not an atomic integer"))
    }

    /// Where this row sits in its holding's chain order.
    fn place(&self) -> (i64, i64, &str) {
        (self.block_position, self.movement_order, &self.id)
    }

    /// Whether `other` states the same effect: every column but the derived
    /// `total`.
    fn same_fact(&self, other: &Self) -> bool {
        let mut this = self.clone();
        this.total.clone_from(&other.total);
        this == *other
    }

    /// Every row `batch` writes ([`LedgerEntry::of_block`]), only for holdings
    /// `watched` watches at that block, never a counterparty's.
    pub(in crate::storage::sqlite) fn from_batch(
        batch: &IndexedBatch,
        watched: &Birthdays,
    ) -> Result<Vec<Self>, IndexError> {
        let mut rows = Vec::new();
        for canonical in batch.blocks() {
            let block_id = Block::try_from(canonical.block.reference())?.id(batch.network());
            for entry in LedgerEntry::of_block(canonical, batch.settlement())? {
                rows.push(Self::from_entry(entry, block_id.clone())?);
            }
        }
        rows.retain(|row| watched.holds((&row.address_id, &row.asset), row.block_position));
        Ok(rows)
    }

    /// Appends every effect of one batch to its holding, in chain order, with
    /// its running total; answers what each holding lost.
    ///
    /// **Identical retries are no-ops.** A row the file already holds under
    /// its id must state the same effect, else the write is terminal: a newly
    /// watched address routinely has rows in its own past, written wherever it
    /// was somebody else's counterparty, and rewriting them blindly would hide
    /// corruption.
    ///
    /// **Forward-only.** When a holding's first new row sits at or before its
    /// tail, the holding's rows from that row's block onward are deleted and
    /// every row this batch has for them is written again, so totals are always
    /// computed in order and never rewritten. Deleted rows the batch does not
    /// carry are reported ([`Lost`]).
    ///
    /// The arithmetic stays in Rust rather than becoming `SUM()`: an amount
    /// here is u256-capable text and SQLite's only integer is 64-bit.
    pub(in crate::storage::sqlite) async fn insert_missing(
        connection: &mut SqliteConnection,
        rows: Vec<Self>,
    ) -> Result<Lost, IndexError> {
        let disagreement = || store("a committed movement disagrees with the stored one");
        let held: BTreeMap<String, Self> = Self::find_held(connection, &rows)
            .await?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        let mut holdings: BTreeMap<(String, String), Vec<Self>> = BTreeMap::new();
        for row in rows {
            let holding = holdings
                .entry((row.address_id.clone(), row.asset.clone()))
                .or_default();
            match holding.iter().find(|other| other.id == row.id) {
                Some(first) if first.same_fact(&row) => {}
                Some(_) => return Err(disagreement()),
                None => holding.push(row),
            }
        }
        let mut lost = Lost::new();
        for (holding, mut rows) in holdings {
            rows.sort_by(|left, right| left.place().cmp(&right.place()));
            let Some(first) = Self::first_fresh(&rows, &held)? else {
                continue;
            };
            let key = (holding.0.as_str(), holding.1.as_str());
            let tail = found(
                Self::get_tail(connection, key).await,
                "failed to read a total",
            )?;
            let rewound =
                tail.is_some_and(|tail| tail.place() >= (first.0, first.1, first.2.as_str()));
            let written: Vec<Self> = if rewound {
                let removed = Self::delete_from(connection, key, first.0).await?;
                let carried: BTreeSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
                let gone = removed
                    .iter()
                    .filter(|row| !carried.contains(row.id.as_str()))
                    .map(|row| row.block_position)
                    .min();
                if let Some(position) = gone {
                    lost.insert(holding.clone(), position);
                }
                rows.into_iter()
                    .filter(|row| row.block_position >= first.0)
                    .collect()
            } else {
                rows.into_iter()
                    .filter(|row| row.place() >= (first.0, first.1, first.2.as_str()))
                    .collect()
            };
            let previous = found(
                Self::get_tail(connection, key).await,
                "failed to read a total",
            )?;
            let mut total = previous.map_or(Ok(BigInt::ZERO), |row| row.running())?;
            let mut written = written;
            for row in &mut written {
                total += row.change()?;
                row.total = total.to_string();
            }
            let count = Self::insert_many(connection, &written).await?;
            if usize::try_from(count).ok() != Some(written.len()) {
                return Err(disagreement());
            }
        }
        Ok(lost)
    }

    /// The place of the first of `rows` (sorted) the file does not hold,
    /// after checking that every row it does hold states the same effect.
    fn first_fresh(
        rows: &[Self],
        held: &BTreeMap<String, Self>,
    ) -> Result<Option<(i64, i64, String)>, IndexError> {
        let mut first = None;
        for row in rows {
            let Some(stored) = held.get(&row.id) else {
                first = first
                    .or_else(|| Some((row.block_position, row.movement_order, row.id.clone())));
                continue;
            };
            if !stored.same_fact(row) {
                return Err(store("a committed movement disagrees with the stored one"));
            }
        }
        Ok(first)
    }

    /// Deletes every row `(address_id, asset)` holds at or after
    /// `block_position`; the rows deleted.
    pub(in crate::storage::sqlite) async fn delete_from(
        connection: &mut SqliteConnection,
        (address_id, asset): (&str, &str),
        block_position: i64,
    ) -> Result<Vec<Self>, IndexError> {
        let query = sqlx::query(
            "DELETE FROM ledger
             WHERE address_id = ?1 AND asset = ?2 AND block_position >= ?3
             RETURNING *",
        )
        .bind(address_id)
        .bind(asset)
        .bind(block_position);
        fetched(connection, query, "failed to delete balance movements").await
    }

    /// The latest row of `(address_id, asset)` in chain order; `RowNotFound`
    /// when it has none.
    pub(in crate::storage::sqlite) async fn get_tail(
        connection: &mut SqliteConnection,
        (address_id, asset): (&str, &str),
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(
            "SELECT * FROM ledger WHERE address_id = ?1 AND asset = ?2
             ORDER BY block_position DESC, movement_order DESC, id DESC LIMIT 1",
        )
        .bind(address_id)
        .bind(asset)
        .fetch_one(connection)
        .await
    }

    /// One holding's balance: its tail's running total, zero when it has no
    /// rows.
    pub(in crate::storage::sqlite) async fn balance(
        connection: &mut SqliteConnection,
        holding: (&str, &str),
    ) -> Result<BigInt, IndexError> {
        let tail = found(
            Self::get_tail(connection, holding).await,
            "failed to read a balance",
        )?;
        tail.map_or(Ok(BigInt::ZERO), |row| row.running())
    }

    /// Insert-if-absent for every row, many to a statement; the number of rows
    /// written.
    async fn insert_many(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<u64, IndexError> {
        Chunks::new(rows, "failed to write balance movements")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT OR IGNORE INTO ledger
                         (id, block_id, transaction_id, address_id, asset, movement_id,
                          block_position, movement_order, kind, diff, total, counterparty) ",
                );
                statement.push_values(chunk, |mut values, row: &Self| {
                    values
                        .push_bind(row.id.as_str())
                        .push_bind(row.block_id.as_str())
                        .push_bind(row.transaction_id.as_deref())
                        .push_bind(row.address_id.as_str())
                        .push_bind(row.asset.as_str())
                        .push_bind(row.movement_id.as_str())
                        .push_bind(row.block_position)
                        .push_bind(row.movement_order)
                        .push_bind(row.effect.tag())
                        .push_bind(row.diff.as_str())
                        .push_bind(row.total.as_str())
                        .push_bind(row.counterparty.as_deref());
                });
                statement
            })
            .await
    }

    /// The rows the file holds under the ids of `rows`, many to a statement,
    /// in no particular order.
    async fn find_held(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(rows, "failed to look up balance movements")
            .fetch(connection, |chunk| {
                let mut statement = QueryBuilder::new("SELECT * FROM ledger WHERE id IN (");
                let mut ids = statement.separated(", ");
                for row in chunk {
                    ids.push_bind(row.id.as_str());
                }
                statement.push(")");
                statement
            })
            .await
    }

    /// `of`'s movements in its asset and the fees its address paid, between
    /// two heights, inclusive, by native transaction id
    /// ([`LedgerEntry::history`]).
    pub(in crate::storage::sqlite) async fn find_effects(
        connection: &mut SqliteConnection,
        of: &dyn Holder,
        (lower, upper): (i64, i64),
    ) -> Result<BTreeMap<String, Effects>, IndexError> {
        let address = of.address();
        let query = sqlx::query(
            "SELECT l.* FROM ledger l JOIN block b ON b.id = l.block_id
             WHERE l.address_id = ?1 AND l.transaction_id IS NOT NULL
               AND ((l.asset = ?2 AND l.kind < 5) OR l.kind = 5)
               AND b.height >= ?3 AND b.height <= ?4",
        )
        .bind(address_id(&address.network, &address.value))
        .bind(of.asset().asset.as_str())
        .bind(lower)
        .bind(upper);
        let rows: Vec<Self> = fetched(connection, query, "failed to read movements").await?;
        let entries = rows
            .iter()
            .map(|row| row.entry(&address.network))
            .collect::<Result<Vec<_>, _>>()?;
        LedgerEntry::history(&entries, of)
    }
}

impl FromRow<'_, SqliteRow> for Ledger {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: column(row, "id")?,
            block_id: column(row, "block_id")?,
            transaction_id: column(row, "transaction_id")?,
            address_id: column(row, "address_id")?,
            asset: column(row, "asset")?,
            movement_id: column(row, "movement_id")?,
            block_position: column(row, "block_position")?,
            movement_order: column(row, "movement_order")?,
            effect: Kind::from_tag(column(row, "kind")?).map_err(raised)?,
            diff: column(row, "diff")?,
            total: column(row, "total")?,
            counterparty: column(row, "counterparty")?,
        })
    }
}
