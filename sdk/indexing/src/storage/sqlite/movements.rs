//! One balance change, for one address, in one asset: the rows a commit writes.
//!
//! Reading them back is `effects.rs` — the fold and the reassembly are one
//! role and this is the other, and they share nothing but the table.
//!
//! **A row is an effect, not a fact.** The fact is the transaction, and it is
//! stored once in `transaction_fact`; what is stored per address is what that
//! transaction did to its subject, with `direction` separate from `kind`. A
//! movement whose endpoints are both the address writes both rows, so the pair
//! nets to zero without either side being lost, and a movement the address only
//! *witnessed* — the ones a fee payer's `history` row used to carry for every
//! other participant — writes none at all.
//!
//! **The network fee is a row on a chain that charges it and no row on a chain
//! that does not.** `FeeSettlement` used to be a parameter every reader was
//! handed; here it is consulted once, at commit, and the answer becomes the
//! stored fact. Bitcoin's fee is already inside `Σ inputs − Σ outputs`, so a
//! `Movements` chain writes nothing and cannot be talked into charging twice.
//!
//! **Transactionless credits are these rows too.** A consensus-layer withdrawal
//! is one address, one asset, one amount and no transaction to name it, which is this row with an
//! empty
//! `transaction_id`. The disjointness that used to be structural — two
//! collections a reader could not confuse — is now an identity argument, and it
//! is exactly as tight: a `MovementId` is unique within its transaction
//! (`Transaction::validate_movements`) and a `BlockCredit`'s is unique
//! within its block (`BlockCredit::validate_all`), and the empty
//! `transaction_id` separates the two namespaces, so no credit can ever collide
//! with a movement.

use base::MovementKind;
use num_bigint::BigUint;
use rusqlite::{Transaction, params};

use super::{Tables, failure, store, tables::Place};
use crate::{Address, BlockCredit, CanonicalTransaction, Decimal, FeeSettlement, IndexError};

/// Value reached the address.
pub(super) const CREDIT: i64 = 0;
/// Value left the address.
const DEBIT: i64 = 1;

/// The network fee, charged beside the movements. Not a [`MovementKind`]: no
/// chain expresses a fee as one, and a tag that could be mistaken for a
/// transfer would put the fee on a history page twice.
pub(super) const FEE: i64 = 5;
/// Value a block moved with no transaction to name it.
pub(super) const WITHDRAWAL: i64 = 6;

/// The identity a fee row carries inside its transaction.
///
/// A movement identity is the chain's own and a fee has none, so one is chosen —
/// and it has to be one **no interpreter can produce**, or a chain emitting a
/// movement under that name would collide with its own transaction's fee and
/// have the whole commit refused as corruption. A readable word like `"fee"` is
/// exactly that collision waiting to happen. The empty string is not:
/// `Transaction::validate_movements` refuses a movement whose identity is
/// empty, so this value is unreachable from outside and the two namespaces are
/// disjoint by construction rather than by convention.
const FEE_ID: &str = "";

/// One subject's credits and debits, in atomic units.
///
/// The pair rather than the difference, everywhere: the difference is not
/// representable until both sides are known, and a store that narrowed to it
/// early would have to decide what to do about a deficit — where the only
/// non-negative answer, zero, is a real debit thrown away.
pub(super) type Totals = (BigUint, BigUint);

/// The empty `transaction_id` of a row no transaction names.
///
/// `NULL` would be the honest spelling and SQLite forbids it: every primary-key
/// column of a `WITHOUT ROWID` table is implicitly `NOT NULL`, and the identity
/// has to be *in* the key or a re-presented block would insert a second copy
/// instead of colliding with the first.
const TRANSACTIONLESS: &str = "";

/// One address as a stored column: the value, or nothing when the movement has
/// no such endpoint.
fn endpoint(value: Option<&Address>) -> Option<&str> {
    value.map(|address| address.value.as_str())
}

/// One row as the two writes and the two reads share it.
struct Entry<'a> {
    address: &'a str,
    asset: &'a str,
    place: &'a Place,
    id: &'a str,
    direction: i64,
    kind: i64,
    amount: &'a Decimal,
    counterparty: Option<&'a str>,
    position: i64,
    order: i64,
}

impl Tables {
    /// Writes every effect `entry` has on `address`, including the fee when the
    /// chain charges one.
    pub(super) fn insert_effects(
        &self,
        transaction: &Transaction<'_>,
        address: &Address,
        entry: &CanonicalTransaction,
        settlement: FeeSettlement,
        place: &Place,
    ) -> Result<(), IndexError> {
        let position = Self::stored(entry.block().position.0)?;
        for (order, movement) in entry.transaction.movements.iter().enumerate() {
            let order = i64::try_from(order).unwrap_or(i64::MAX);
            let (from, to) = (endpoint(movement.from()), endpoint(movement.to()));
            let effect = Entry {
                address: &address.value,
                asset: &movement.asset.asset,
                place,
                id: &movement.id.0,
                direction: CREDIT,
                kind: Self::tag(movement.kind()),
                amount: &movement.amount,
                counterparty: from,
                position,
                order,
            };
            if to == Some(address.value.as_str()) {
                self.write(transaction, &effect)?;
            }
            if from == Some(address.value.as_str()) {
                let debit = Entry {
                    direction: DEBIT,
                    counterparty: to,
                    ..effect
                };
                self.write(transaction, &debit)?;
            }
        }
        self.charge(transaction, address, entry, settlement, place)
    }

    /// Writes the fee row, when this chain charges its fee beside the movements
    /// and this address is the payer.
    ///
    /// One row per transaction per payer, never one per participating address:
    /// a Bitcoin-style fan-out names a dozen addresses on one transaction, and a
    /// fee written under each of them would charge the payer once for every
    /// other participant.
    fn charge(
        &self,
        transaction: &Transaction<'_>,
        address: &Address,
        entry: &CanonicalTransaction,
        settlement: FeeSettlement,
        place: &Place,
    ) -> Result<(), IndexError> {
        let charged = entry.transaction.fee.as_ref().filter(|fee| {
            settlement == FeeSettlement::Charged && fee.payer.as_ref() == Some(address)
        });
        let Some(fee) = charged else {
            return Ok(());
        };
        let effect = Entry {
            address: &address.value,
            asset: &fee.asset.asset,
            place,
            id: FEE_ID,
            direction: DEBIT,
            kind: FEE,
            amount: &fee.amount,
            counterparty: None,
            position: Self::stored(entry.block().position.0)?,
            order: i64::MAX,
        };
        self.write(transaction, &effect)
    }

    /// Writes one transactionless credit under the address it credits.
    pub(super) fn insert_credit(
        &self,
        transaction: &Transaction<'_>,
        credit: &BlockCredit,
        height: i64,
        position: i64,
    ) -> Result<(), IndexError> {
        let place = Place {
            height,
            order: 0,
            id: TRANSACTIONLESS.to_owned(),
        };
        let effect = Entry {
            address: &credit.to.value,
            asset: &credit.asset.asset,
            place: &place,
            id: &credit.id.0,
            direction: CREDIT,
            kind: WITHDRAWAL,
            amount: &credit.amount,
            counterparty: None,
            position,
            order: 0,
        };
        self.write(transaction, &effect)
    }

    /// Writes every transactionless credit of one block, under the same rule as
    /// its transactions: one already present is verified, one that differs is
    /// corruption.
    pub(super) fn refill_credits(
        &self,
        transaction: &Transaction<'_>,
        credits: &[BlockCredit],
        block: (i64, i64),
    ) -> Result<(), IndexError> {
        let (height, position) = block;
        credits
            .iter()
            .try_for_each(|credit| self.insert_credit(transaction, credit, height, position))
    }

    /// Insert-if-absent, verify-if-present, for one row.
    ///
    /// The primary key decides which of the two this is inside the insert, so
    /// the decision costs no read on the common path — and a row the file
    /// already holds is compared rather than rewritten, because one
    /// `(address, asset, block, transaction, movement, direction)` cannot
    /// honestly have two amounts.
    fn write(&self, transaction: &Transaction<'_>, effect: &Entry<'_>) -> Result<(), IndexError> {
        let written = transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO balance_movement
                     (chain, network, address, asset, block_height, transaction_id,
                      movement_id, direction, block_position, movement_order, kind,
                      amount, counterparty)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    self.chain(),
                    self.name(),
                    effect.address,
                    effect.asset,
                    effect.place.height,
                    effect.place.id,
                    effect.id,
                    effect.direction,
                    effect.position,
                    effect.order,
                    effect.kind,
                    effect.amount.to_string(),
                    effect.counterparty,
                ])
            })
            .map_err(|error| failure(&error, "failed to write a balance movement"))?;
        if written == 0 {
            return self.verify_effect(transaction, effect);
        }
        self.invalidate_evidence(
            transaction,
            (effect.address, effect.asset),
            effect.place.height,
        )?;
        let units = effect
            .amount
            .to_atomic(0)
            .map_err(|_| store("an effect is not an atomic integer"))?;
        let delta = num_bigint::BigInt::from(units);
        self.accrue(
            transaction,
            (effect.address, effect.asset),
            if effect.direction == CREDIT {
                delta
            } else {
                -delta
            },
        )
    }

    /// Checks that the row the file already holds is the row being written.
    fn verify_effect(
        &self,
        transaction: &Transaction<'_>,
        effect: &Entry<'_>,
    ) -> Result<(), IndexError> {
        let held: (String, i64, Option<String>) = transaction
            .query_row(
                "SELECT amount, kind, counterparty FROM balance_movement
                 WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4
                   AND block_height = ?5 AND transaction_id = ?6
                   AND movement_id = ?7 AND direction = ?8",
                params![
                    self.chain(),
                    self.name(),
                    effect.address,
                    effect.asset,
                    effect.place.height,
                    effect.place.id,
                    effect.id,
                    effect.direction,
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| failure(&error, "failed to look up a balance movement"))?;
        let presented = (
            effect.amount.to_string(),
            effect.kind,
            effect.counterparty.map(ToOwned::to_owned),
        );
        if held == presented {
            return Ok(());
        }
        Err(store("a committed movement disagrees with the stored one"))
    }

    /// The stored tag of one movement kind.
    ///
    /// A number rather than a word, and the vocabulary is [`MovementKind`]
    /// rather than this module's own for the five it names: the stored spelling
    /// of a closed vocabulary belongs to the type that closes it.
    pub(super) fn tag(kind: MovementKind) -> i64 {
        match kind {
            MovementKind::Transfer => 0,
            MovementKind::Input => 1,
            MovementKind::Output => 2,
            MovementKind::Mint => 3,
            MovementKind::Burn => 4,
        }
    }
}
