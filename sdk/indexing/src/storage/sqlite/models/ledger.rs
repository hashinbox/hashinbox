//! `ledger`: one balance change, for one address, in one asset.
//!
//! **A row is an effect, not a fact.** The fact is the transaction, stored once
//! in `"transaction"`; what is stored per address is what that transaction did
//! to its subject, with `direction` separate from `kind`.
//!
//! **Transactionless credits are these rows too.** A consensus-layer withdrawal
//! is one address, one asset, one amount and no transaction to name it, which
//! is this row with an empty `transaction_id`. A `MovementId` is unique within
//! its transaction (`Transaction::validate_movements`) and a `BlockCredit`'s
//! within its block (`BlockCredit::validate_all`), and the empty
//! `transaction_id` separates the two namespaces, so no credit can ever collide
//! with a movement.

use std::collections::BTreeMap;

use base::MovementKind;
use num_bigint::{BigInt, BigUint};
use sqlx::{FromRow, QueryBuilder, SqliteConnection, sqlite::SqliteRow};

use super::{
    Chunks, amount, column, deleted, each, failure, framed, network, raised, store, stored,
};
use crate::{
    Address, BlockCredit, FeeSettlement, IndexError, IndexedBatch, Network, NetworkFee,
    Transaction, ValueMovement,
};

/// One subject's credits and debits, in atomic units.
///
/// The pair rather than the difference, everywhere: the difference is not
/// representable until both sides are known, and a store that narrowed to it
/// early would have to decide what to do about a deficit — where the only
/// non-negative answer, zero, is a real debit thrown away.
pub(in crate::storage::sqlite) type Totals = (BigUint, BigUint);

/// What the rows one commit wrote change each `(address, asset)` holding's
/// signed net by.
pub(in crate::storage::sqlite) type Deltas = BTreeMap<(String, String), BigInt>;

/// What one `ledger` row is the only row for: its primary key.
type LedgerKey<'a> = (
    &'a Network,
    &'a str,
    &'a str,
    i64,
    &'a str,
    &'a str,
    Direction,
);

/// The empty `transaction_id` of a row no transaction names, and the empty
/// `movement_id` of a fee row inside its transaction.
///
/// `NULL` would be the honest spelling and SQLite forbids it: every primary-key
/// column of a `WITHOUT ROWID` table is implicitly `NOT NULL`. The empty string
/// is unreachable from outside: `Transaction::validate_movements` refuses a
/// movement whose identity is empty, so a fee can never collide with a
/// movement of its own transaction.
const UNNAMED: &str = "";

/// Which way value crossed the address: `direction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Direction {
    /// `0`: value reached the address.
    Credit,
    /// `1`: value left the address.
    Debit,
}

impl Direction {
    /// The direction one stored tag names.
    fn from_tag(tag: i64) -> Result<Self, IndexError> {
        match tag {
            0 => Ok(Self::Credit),
            1 => Ok(Self::Debit),
            _ => Err(store("a stored movement has an unknown direction")),
        }
    }

    const fn tag(self) -> i64 {
        match self {
            Self::Credit => 0,
            Self::Debit => 1,
        }
    }
}

/// What the row records: `kind`.
///
/// The stored spelling of the five movement kinds is [`MovementKind`]'s order;
/// the fee and the withdrawal are balance changes without being movements, so
/// they are this store's own two tags after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    /// `0..=4`: a movement of its transaction.
    Movement(MovementKind),
    /// `5`: the network fee, charged beside the movements.
    Fee,
    /// `6`: value a block moved with no transaction to name it.
    Withdrawal,
}

impl Effect {
    /// The effect one stored tag names.
    fn from_tag(tag: i64) -> Result<Self, IndexError> {
        Ok(match tag {
            0 => Self::Movement(MovementKind::Transfer),
            1 => Self::Movement(MovementKind::Input),
            2 => Self::Movement(MovementKind::Output),
            3 => Self::Movement(MovementKind::Mint),
            4 => Self::Movement(MovementKind::Burn),
            5 => Self::Fee,
            6 => Self::Withdrawal,
            _ => return Err(store("a stored movement has an unknown kind")),
        })
    }

    const fn tag(self) -> i64 {
        match self {
            Self::Movement(MovementKind::Transfer) => 0,
            Self::Movement(MovementKind::Input) => 1,
            Self::Movement(MovementKind::Output) => 2,
            Self::Movement(MovementKind::Mint) => 3,
            Self::Movement(MovementKind::Burn) => 4,
            Self::Fee => 5,
            Self::Withdrawal => 6,
        }
    }
}

/// One `ledger` row.
///
/// Equality is the whole row: one `(address, asset, block, transaction,
/// movement, direction)` cannot honestly have two amounts, kinds or
/// counterparties, so a re-presented effect is verified by comparing rows.
///
#[derive(Debug, PartialEq, Eq)]
pub(in crate::storage::sqlite) struct Ledger {
    network: Network,
    address: String,
    asset: String,
    block_height: i64,
    transaction_id: String,
    movement_id: String,
    direction: Direction,
    block_position: i64,
    movement_order: i64,
    effect: Effect,
    amount: String,
    counterparty: Option<String>,
}

impl Ledger {
    /// What `movement`, the `order`th of `entry`, did to `address` in
    /// `direction`; `counterparty` is the movement's other endpoint.
    fn movement(
        address: &Address,
        entry: &Transaction,
        movement: (usize, &ValueMovement),
        direction: Direction,
    ) -> Result<Self, IndexError> {
        let (order, movement) = movement;
        let other = match direction {
            Direction::Credit => movement.from(),
            Direction::Debit => movement.to(),
        };
        Ok(Self {
            network: address.network.clone(),
            address: address.value.clone(),
            asset: movement.asset.asset.clone(),
            block_height: stored(entry.block().height.0)?,
            transaction_id: entry.id.id.as_str().to_owned(),
            movement_id: movement.id.0.clone(),
            direction,
            block_position: stored(entry.block().position.0)?,
            movement_order: i64::try_from(order).unwrap_or(i64::MAX),
            effect: Effect::Movement(movement.kind()),
            amount: movement.amount.to_string(),
            counterparty: other.map(|other| other.value.clone()),
        })
    }

    /// The fee `entry` charged its payer, `address`.
    ///
    /// One row per transaction per payer, never one per participating address:
    /// a Bitcoin-style fan-out names a dozen addresses on one transaction, and a
    /// fee written under each of them would charge the payer once for every
    /// other participant.
    fn fee(address: &Address, entry: &Transaction, fee: &NetworkFee) -> Result<Self, IndexError> {
        Ok(Self {
            network: address.network.clone(),
            address: address.value.clone(),
            asset: fee.asset.asset.clone(),
            block_height: stored(entry.block().height.0)?,
            transaction_id: entry.id.id.as_str().to_owned(),
            movement_id: UNNAMED.to_owned(),
            direction: Direction::Debit,
            block_position: stored(entry.block().position.0)?,
            movement_order: i64::MAX,
            effect: Effect::Fee,
            amount: fee.amount.to_string(),
            counterparty: None,
        })
    }

    /// One transactionless credit, in the block at `(height, position)`.
    fn withdrawal(credit: &BlockCredit, block: (i64, i64)) -> Self {
        let (height, position) = block;
        Self {
            network: credit.to.network.clone(),
            address: credit.to.value.clone(),
            asset: credit.asset.asset.clone(),
            block_height: height,
            transaction_id: UNNAMED.to_owned(),
            movement_id: credit.id.0.clone(),
            direction: Direction::Credit,
            block_position: position,
            movement_order: 0,
            effect: Effect::Withdrawal,
            amount: credit.amount.to_string(),
            counterparty: None,
        }
    }

    /// Every effect `entry` has on `address`: a credit where the address
    /// received a movement, a debit where it sent one, both for a movement it
    /// sent itself, and the fee when the chain charges it beside the movements
    /// and this address paid it.
    ///
    /// **The network fee is a row on a chain that charges it and no row on a
    /// chain that does not.** Bitcoin's fee is already inside
    /// `Σ inputs − Σ outputs`, so a `Movements` chain writes nothing and cannot
    /// be talked into charging twice.
    fn effects(
        address: &Address,
        entry: &Transaction,
        settlement: FeeSettlement,
    ) -> Result<Vec<Self>, IndexError> {
        let mut effects = Vec::new();
        for movement in entry.movements.iter().enumerate() {
            let (_, value) = movement;
            if value.to().map(|to| &to.value) == Some(&address.value) {
                effects.push(Self::movement(address, entry, movement, Direction::Credit)?);
            }
            if value.from().map(|from| &from.value) == Some(&address.value) {
                effects.push(Self::movement(address, entry, movement, Direction::Debit)?);
            }
        }
        let charged = entry.fee.as_ref().filter(|fee| {
            settlement == FeeSettlement::Charged && fee.payer.as_ref() == Some(address)
        });
        if let Some(fee) = charged {
            effects.push(Self::fee(address, entry, fee)?);
        }
        Ok(effects)
    }

    /// The `(address, asset)` holding this row changes.
    fn subject(&self) -> (&str, &str) {
        (&self.address, &self.asset)
    }

    /// The amount in atomic units, unsigned: the fold adds it to its side.
    fn units(&self) -> Result<BigUint, IndexError> {
        amount(&self.amount)?
            .to_atomic(0)
            .map_err(|_| store("a stored amount is not an atomic integer"))
    }

    /// Adds this row to the side of `totals` its direction names.
    fn fold(&self, totals: &mut Totals) -> Result<(), IndexError> {
        let units = self.units()?;
        match self.direction {
            Direction::Credit => totals.0 += units,
            Direction::Debit => totals.1 += units,
        }
        Ok(())
    }

    /// What this row changes its holding's signed net by.
    fn delta(&self) -> Result<BigInt, IndexError> {
        let units = BigInt::from(self.units()?);
        Ok(match self.direction {
            Direction::Credit => units,
            Direction::Debit => -units,
        })
    }

    /// The primary key: what one row is the only row for.
    fn key(&self) -> LedgerKey<'_> {
        (
            &self.network,
            &self.address,
            &self.asset,
            self.block_height,
            &self.transaction_id,
            &self.movement_id,
            self.direction,
        )
    }

    /// Every row `batch` writes, block by block: each transaction's effects on
    /// every address that took part in it, then the block's credits.
    ///
    /// The transaction's own place is `(height, transaction id)`, which is a
    /// function of the block rather than of this batch: a block is reported
    /// with only the transactions that touched a selected address, so anything
    /// derived from the *position* of a transaction in that list would differ
    /// between two selections of the same block.
    pub(in crate::storage::sqlite) fn from_batch(
        batch: &IndexedBatch,
    ) -> Result<Vec<Self>, IndexError> {
        let mut rows = Vec::new();
        for canonical in batch.blocks() {
            let participants = canonical.block.transactions().iter().flat_map(|entry| {
                let addresses = entry.addresses().into_iter();
                addresses.map(move |address| (address, entry))
            });
            for (address, entry) in participants {
                rows.extend(Self::effects(&address, entry, batch.settlement())?);
            }
            let position = stored(canonical.block.reference().position.0)?;
            let height = stored(canonical.block.reference().height.0)?;
            rows.extend(
                canonical
                    .credits
                    .iter()
                    .map(|credit| Self::withdrawal(credit, (height, position))),
            );
        }
        Ok(rows)
    }

    /// Insert-if-absent, verify-if-present, for every effect of one batch, and
    /// the change the rows it wrote make to each holding.
    ///
    /// **One rule, both directions.** A newly watched address routinely already
    /// has rows in its own past: an effect is written for *every* endpoint of
    /// every movement, so the address has one wherever it was somebody else's
    /// counterparty. Refusing those would loop forever, and rewriting them
    /// blindly would hide corruption, so a row that differs is terminal.
    ///
    /// The rows the file already holds under the batch's keys are read first,
    /// many keys to a statement, inside the write transaction that holds the
    /// file's lock, so what is absent here is exactly what the insert writes,
    /// and only those rows change a watched total. A row the file already
    /// holds is compared rather than rewritten, because one
    /// `(address, asset, block, transaction, movement, direction)` cannot
    /// honestly have two amounts; a key the batch repeats is compared with its
    /// first occurrence, which is the row the file then holds.
    pub(in crate::storage::sqlite) async fn insert_missing(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<Deltas, IndexError> {
        let disagreement = || store("a committed movement disagrees with the stored one");
        let held = Self::find_held(connection, rows).await?;
        let mut known: BTreeMap<_, &Self> = held.iter().map(|row| (row.key(), row)).collect();
        let mut fresh = Vec::new();
        for row in rows {
            match known.get(&row.key()) {
                Some(stored) if *stored == row => {}
                Some(_) => return Err(disagreement()),
                None => {
                    known.insert(row.key(), row);
                    fresh.push(row);
                }
            }
        }
        // Every fresh row is absent and unique, so all of them are written; a
        // row the insert still ignored was refused for something other than
        // its key.
        let written = Self::insert_many(connection, &fresh).await?;
        if usize::try_from(written).ok() != Some(fresh.len()) {
            return Err(disagreement());
        }
        let mut deltas = Deltas::new();
        for row in fresh {
            let (address, asset) = row.subject();
            *deltas
                .entry((address.to_owned(), asset.to_owned()))
                .or_default() += row.delta()?;
        }
        Ok(deltas)
    }

    /// Insert-if-absent for every row, many to a statement; the number of rows
    /// written.
    async fn insert_many(
        connection: &mut SqliteConnection,
        rows: &[&Self],
    ) -> Result<u64, IndexError> {
        Chunks::new(rows, "failed to write balance movements")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT OR IGNORE INTO ledger
                         (chain, network, address, asset, block_height, transaction_id,
                          movement_id, direction, block_position, movement_order, kind,
                          amount, counterparty) ",
                );
                statement.push_values(chunk, |mut values, row: &&Self| {
                    values
                        .push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.address.as_str())
                        .push_bind(row.asset.as_str())
                        .push_bind(row.block_height)
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.movement_id.as_str())
                        .push_bind(row.direction.tag())
                        .push_bind(row.block_position)
                        .push_bind(row.movement_order)
                        .push_bind(row.effect.tag())
                        .push_bind(row.amount.as_str())
                        .push_bind(row.counterparty.as_deref());
                });
                statement
            })
            .await
    }

    /// The rows the file holds under the keys of `rows`, many keys to a
    /// statement, in no particular order.
    async fn find_held(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(rows, "failed to look up balance movements")
            .fetch(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "SELECT * FROM ledger
                     WHERE (chain, network, address, asset, block_height, transaction_id,
                            movement_id, direction) IN (",
                );
                statement.push_values(chunk, |mut key, row: &Self| {
                    key.push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.address.as_str())
                        .push_bind(row.asset.as_str())
                        .push_bind(row.block_height)
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.movement_id.as_str())
                        .push_bind(row.direction.tag());
                });
                statement.push(")");
                statement
            })
            .await
    }

    /// Every credit and debit of one subject, summed.
    ///
    /// **One statement, streamed, and no cursor.** The rows are read through a
    /// running statement inside the caller's own transaction, so this costs one
    /// row at a time whatever the subject's history is, and there is no resume
    /// position to get wrong. A paginated form keyed its cursor on
    /// `(height, transaction, movement)` and therefore could not resume inside
    /// a same-address transfer, whose credit and debit share every one of
    /// those columns and differ only in `direction` — a page boundary landing
    /// between them dropped the debit and overstated the subject.
    ///
    /// The arithmetic stays in Rust rather than becoming `SUM()`: an amount here
    /// is u256-capable canonical text and SQLite's only integer is a signed
    /// 64-bit one.
    pub(in crate::storage::sqlite) async fn total(
        connection: &mut SqliteConnection,
        network: &Network,
        (address, asset): (&str, &str),
    ) -> Result<Totals, IndexError> {
        let mut totals = (BigUint::ZERO, BigUint::ZERO);
        let context = "failed to fold balance movements";
        let query = framed(
            "SELECT * FROM ledger
             WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4",
            network,
        )
        .bind(address)
        .bind(asset);
        each(connection, query, context, |row| {
            Self::from_row(row)
                .map_err(|error| failure(&error, context))?
                .fold(&mut totals)
        })
        .await?;
        Ok(totals)
    }

    /// One subject's signed net, folded from every row: what an unwatched
    /// holding reads, what an observed one is seeded with, and what a
    /// persisted total is verified against.
    pub(in crate::storage::sqlite) async fn net(
        connection: &mut SqliteConnection,
        network: &Network,
        subject: (&str, &str),
    ) -> Result<BigInt, IndexError> {
        let (credits, debits) = Self::total(connection, network, subject).await?;
        Ok(BigInt::from(credits) - BigInt::from(debits))
    }

    /// Removes every row of `network`: `(credits, movements)`, the
    /// transactionless rows counted first and apart, because an operator
    /// counts a withdrawal and a transaction's movement as different facts.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<(u64, u64), IndexError> {
        let credits = deleted(
            connection,
            network,
            "DELETE FROM ledger WHERE chain = ?1 AND network = ?2 AND transaction_id = ''",
        )
        .await?;
        let movements = deleted(
            connection,
            network,
            "DELETE FROM ledger WHERE chain = ?1 AND network = ?2",
        )
        .await?;
        Ok((credits, movements))
    }
}

impl FromRow<'_, SqliteRow> for Ledger {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            network: network(row)?,
            address: column(row, "address")?,
            asset: column(row, "asset")?,
            block_height: column(row, "block_height")?,
            transaction_id: column(row, "transaction_id")?,
            movement_id: column(row, "movement_id")?,
            direction: Direction::from_tag(column(row, "direction")?).map_err(raised)?,
            block_position: column(row, "block_position")?,
            movement_order: column(row, "movement_order")?,
            effect: Effect::from_tag(column(row, "kind")?).map_err(raised)?,
            amount: column(row, "amount")?,
            counterparty: column(row, "counterparty")?,
        })
    }
}
