//! `ledger`: one balance change, for one address, in one asset.
//!
//! **A row is an effect, not a fact.** The fact is the transaction, stored once
//! in `"transaction"`; what is stored per address is what that transaction did
//! to its subject, with `direction` separate from `kind`. A network fee is one
//! of these effects — a debit of the payer, naming its transaction — and is
//! the only place a fee is stored: history reads it back from here.
//!
//! **Transactionless credits are these rows too.** A consensus-layer withdrawal
//! is one address, one asset, one amount and no transaction to name it, which
//! is this row with a null `transaction_id`. Its id carries an empty
//! transaction part, which no transaction has
//! (`Transaction::validate_identity`), so no credit can ever collide with a
//! movement.

use std::collections::BTreeMap;

use base::MovementKind;
use num_bigint::{BigInt, BigUint};
use sqlx::{FromRow, QueryBuilder, SqliteConnection, sqlite::SqliteRow};

use super::{
    Block, Chunks, address_id, amount, canonical_id, column, each, failure, fetched, raised, store,
};
use crate::{
    Address, AssetId, BlockCredit, BlockRef, FeeSettlement, Holder, IndexError, IndexedBatch,
    MovementId, Network, NetworkFee, Transaction, ValueMovement,
};

/// One subject's credits and debits, in atomic units.
///
/// The pair rather than the difference, everywhere: the difference is not
/// representable until both sides are known, and a store that narrowed to it
/// early would have to decide what to do about a deficit — where the only
/// non-negative answer, zero, is a real debit thrown away.
pub(in crate::storage::sqlite) type Totals = (BigUint, BigUint);

/// What the rows one commit wrote change each `(address id, asset)` holding's
/// signed net by.
pub(in crate::storage::sqlite) type Deltas = BTreeMap<(String, String), BigInt>;

/// What one transaction did to one address, as history shows it: its
/// movements, and the fee it was charged.
pub(in crate::storage::sqlite) type Effects = (Vec<ValueMovement>, Option<NetworkFee>);

/// The `movement_id` of a fee charged beside the movements.
///
/// `Transaction::validate_movements` refuses a movement whose identity is
/// empty, so a fee can never collide with a movement of its own transaction.
const CHARGED: &str = "";

/// The `movement_id` of a fee the movements already contain (Bitcoin's
/// inputs minus outputs): recorded so history shows it, never folded into a
/// balance again. A movement id is the chain's own, and no chain names a
/// movement this, because a fee row has `kind` 5 and a movement does not.
const EMBEDDED: &str = "embedded";

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
/// Equality is the whole row: one id — `(block, transaction, movement,
/// address, asset, direction)` — cannot honestly have two amounts, kinds or
/// counterparties, so a re-presented effect is verified by comparing rows.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::storage::sqlite) struct Ledger {
    id: String,
    block_id: String,
    transaction_id: Option<String>,
    address_id: String,
    asset: String,
    movement_id: String,
    movement_order: i64,
    direction: Direction,
    effect: Effect,
    amount: String,
    counterparty: Option<String>,
}

/// Where one row sits: its network, its block's id, and its transaction's
/// native id (`None` for a transactionless credit).
struct Place<'a> {
    network: &'a Network,
    block: &'a BlockRef,
    transaction: Option<&'a str>,
}

impl Ledger {
    /// The row of one effect on `(address, asset)` at `place`, its id derived
    /// from every part of its key; the caller states its amount and
    /// counterparty.
    fn at(
        place: &Place<'_>,
        (address, asset): (&str, &str),
        movement: (&str, i64),
        direction: Direction,
        effect: Effect,
    ) -> Result<Self, IndexError> {
        let network = place.network;
        let block_id = Block::try_from(place.block)?.id(network);
        let hash = hex::encode(&place.block.hash.0);
        let (movement_id, movement_order) = movement;
        let tag = direction.tag().to_string();
        let id = canonical_id(&[
            network.chain.as_str(),
            &network.name,
            &hash,
            place.transaction.unwrap_or_default(),
            movement_id,
            address,
            asset,
            &tag,
        ]);
        Ok(Self {
            id,
            block_id,
            transaction_id: place
                .transaction
                .map(|id| canonical_id(&[network.chain.as_str(), &network.name, id])),
            address_id: address_id(network, address),
            asset: asset.to_owned(),
            movement_id: movement_id.to_owned(),
            movement_order,
            direction,
            effect,
            amount: String::new(),
            counterparty: None,
        })
    }

    /// Every effect `entry` has on `address`: a credit where the address
    /// received a movement, a debit where it sent one, both for a movement it
    /// sent itself, and the fee when the chain charges it beside the movements
    /// and this address paid it.
    ///
    /// **The network fee is always a row, and folded only where it is
    /// charged.** Bitcoin's fee is already inside `Σ inputs − Σ outputs`, so an
    /// `Embedded` chain's fee row is marked [`EMBEDDED`] and adds nothing to a
    /// balance: it cannot be talked into charging twice.
    fn effects(
        address: &Address,
        entry: &Transaction,
        settlement: FeeSettlement,
    ) -> Result<Vec<Self>, IndexError> {
        let place = Place {
            network: &address.network,
            block: entry.block(),
            transaction: Some(entry.id.id.as_str()),
        };
        let mut effects = Vec::new();
        for (order, movement) in entry.movements.iter().enumerate() {
            let order = i64::try_from(order).unwrap_or(i64::MAX);
            let sides = [
                (Direction::Credit, movement.to(), movement.from()),
                (Direction::Debit, movement.from(), movement.to()),
            ];
            for (direction, side, other) in sides {
                if side.map(|side| &side.value) != Some(&address.value) {
                    continue;
                }
                effects.push(Self {
                    amount: movement.amount.to_string(),
                    counterparty: other.map(|other| other.value.clone()),
                    ..Self::at(
                        &place,
                        (&address.value, &movement.asset.asset),
                        (&movement.id.0, order),
                        direction,
                        Effect::Movement(movement.kind()),
                    )?
                });
            }
        }
        // One row per transaction per payer, never one per participating
        // address: a fan-out names a dozen addresses on one transaction, and a
        // fee written under each of them would charge the payer once for every
        // other participant.
        let paid = entry
            .fee
            .as_ref()
            .filter(|fee| fee.payer.as_ref() == Some(address));
        if let Some(fee) = paid {
            let marker = match settlement {
                FeeSettlement::Charged => CHARGED,
                FeeSettlement::Embedded => EMBEDDED,
            };
            effects.push(Self {
                amount: fee.amount.to_string(),
                ..Self::at(
                    &place,
                    (&address.value, &fee.asset.asset),
                    (marker, i64::MAX),
                    Direction::Debit,
                    Effect::Fee,
                )?
            });
        }
        Ok(effects)
    }

    /// One transactionless credit, in `block`.
    fn withdrawal(credit: &BlockCredit, block: &BlockRef) -> Result<Self, IndexError> {
        let place = Place {
            network: &credit.to.network,
            block,
            transaction: None,
        };
        Ok(Self {
            amount: credit.amount.to_string(),
            ..Self::at(
                &place,
                (&credit.to.value, &credit.asset.asset),
                (&credit.id.0, 0),
                Direction::Credit,
                Effect::Withdrawal,
            )?
        })
    }

    /// The amount in atomic units, unsigned, that the fold adds to its side:
    /// none for a fee the movements already contain.
    fn units(&self) -> Result<BigUint, IndexError> {
        if self.effect == Effect::Fee && self.movement_id == EMBEDDED {
            return Ok(BigUint::ZERO);
        }
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

    /// The fee this row charged `payer`, as history shows it.
    fn fee(&self, payer: &Address) -> Result<NetworkFee, IndexError> {
        Ok(NetworkFee {
            asset: AssetId {
                chain: payer.network.chain.clone(),
                asset: self.asset.clone(),
            },
            amount: amount(&self.amount)?,
            payer: Some(payer.clone()),
        })
    }

    /// Every row `batch` writes, block by block: each transaction's effects on
    /// every address that took part in it, then the block's credits.
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
            for credit in &canonical.credits {
                rows.push(Self::withdrawal(credit, canonical.block.reference())?);
            }
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
    /// The rows the file already holds under the batch's ids are read first,
    /// many to a statement, inside the write transaction that holds the
    /// file's lock, so what is absent here is exactly what the insert writes,
    /// and only those rows change a watched total. A id the batch repeats is
    /// compared with its first occurrence, which is the row the file then
    /// holds.
    pub(in crate::storage::sqlite) async fn insert_missing(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<Deltas, IndexError> {
        let disagreement = || store("a committed movement disagrees with the stored one");
        let held = Self::find_held(connection, rows).await?;
        let mut known: BTreeMap<&str, &Self> =
            held.iter().map(|row| (row.id.as_str(), row)).collect();
        let mut fresh = Vec::new();
        for row in rows {
            match known.get(row.id.as_str()) {
                Some(stored) if *stored == row => {}
                Some(_) => return Err(disagreement()),
                None => {
                    known.insert(&row.id, row);
                    fresh.push(row);
                }
            }
        }
        // Every fresh row is absent and unique, so all of them are written; a
        // row the insert still ignored was refused for something other than
        // its id.
        let written = Self::insert_many(connection, &fresh).await?;
        if usize::try_from(written).ok() != Some(fresh.len()) {
            return Err(disagreement());
        }
        let mut deltas = Deltas::new();
        for row in fresh {
            *deltas
                .entry((row.address_id.clone(), row.asset.clone()))
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
                         (id, block_id, transaction_id, address_id, asset, movement_id,
                          movement_order, direction, kind, amount, counterparty) ",
                );
                statement.push_values(chunk, |mut values, row: &&Self| {
                    values
                        .push_bind(row.id.as_str())
                        .push_bind(row.block_id.as_str())
                        .push_bind(row.transaction_id.as_deref())
                        .push_bind(row.address_id.as_str())
                        .push_bind(row.asset.as_str())
                        .push_bind(row.movement_id.as_str())
                        .push_bind(row.movement_order)
                        .push_bind(row.direction.tag())
                        .push_bind(row.effect.tag())
                        .push_bind(row.amount.as_str())
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

    /// Every credit and debit of one holding, summed.
    ///
    /// **One statement, streamed, and no cursor.** The rows are read through a
    /// running statement inside the caller's own transaction, so this costs one
    /// row at a time whatever the holding's history is.
    ///
    /// The arithmetic stays in Rust rather than becoming `SUM()`: an amount here
    /// is u256-capable canonical text and SQLite's only integer is a signed
    /// 64-bit one.
    pub(in crate::storage::sqlite) async fn total(
        connection: &mut SqliteConnection,
        (address_id, asset): (&str, &str),
    ) -> Result<Totals, IndexError> {
        let mut totals = (BigUint::ZERO, BigUint::ZERO);
        let context = "failed to fold balance movements";
        let query = sqlx::query("SELECT * FROM ledger WHERE address_id = ?1 AND asset = ?2")
            .bind(address_id)
            .bind(asset);
        each(connection, query, context, |row| {
            Self::from_row(row)
                .map_err(|error| failure(&error, context))?
                .fold(&mut totals)
        })
        .await?;
        Ok(totals)
    }

    /// One holding's signed net, folded from every row: what an unwatched
    /// holding reads, and what a new observation's total is seeded with.
    pub(in crate::storage::sqlite) async fn net(
        connection: &mut SqliteConnection,
        holding: (&str, &str),
    ) -> Result<BigInt, IndexError> {
        let (credits, debits) = Self::total(connection, holding).await?;
        Ok(BigInt::from(credits) - BigInt::from(debits))
    }

    /// `of`'s movements in its asset and the fees its address paid, between
    /// two heights, inclusive, by transaction id in stored order. The two rows
    /// a same-address transfer wrote, a credit and a debit, are folded back
    /// into its one movement. Transactionless credits are not here.
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
               AND b.height >= ?3 AND b.height <= ?4
             ORDER BY b.height, l.transaction_id, l.movement_order, l.movement_id",
        )
        .bind(address_id(&address.network, &address.value))
        .bind(of.asset().asset.as_str())
        .bind(lower)
        .bind(upper);
        let rows: Vec<Self> = fetched(connection, query, "failed to read movements").await?;
        let mut grouped: BTreeMap<String, Effects> = BTreeMap::new();
        let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
        for row in rows {
            let transaction = row.transaction_id.clone().unwrap_or_default();
            let held = grouped.entry(transaction.clone()).or_default();
            let kind = match row.effect {
                Effect::Movement(kind) => kind,
                Effect::Fee => {
                    held.1 = Some(row.fee(address)?);
                    continue;
                }
                Effect::Withdrawal => return Err(store("a stored movement has an unknown kind")),
            };
            let mine = Some(address.clone());
            let other = row
                .counterparty
                .map(|value| Address::new(address.network.clone(), value));
            let key = (transaction, row.movement_id.clone());
            let (from, to, at) = match (seen.get(&key), row.direction) {
                // The other direction of the same movement: a self-transfer.
                (Some(at), _) => (mine.clone(), mine, Some(*at)),
                (None, Direction::Credit) => (other, mine, None),
                (None, Direction::Debit) => (mine, other, None),
            };
            let value = ValueMovement::stored(
                MovementId(row.movement_id),
                of.asset().clone(),
                amount(&row.amount)?,
                kind,
                from,
                to,
            )?;
            match at {
                Some(at) => held.0[at] = value,
                None => {
                    seen.insert(key, held.0.len());
                    held.0.push(value);
                }
            }
        }
        Ok(grouped)
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
            movement_order: column(row, "movement_order")?,
            direction: Direction::from_tag(column(row, "direction")?).map_err(raised)?,
            effect: Effect::from_tag(column(row, "kind")?).map_err(raised)?,
            amount: column(row, "amount")?,
            counterparty: column(row, "counterparty")?,
        })
    }
}
