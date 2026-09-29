//! Reading one address's balance changes back: what they add up to, and what
//! they say the transaction did.
//!
//! Split from `movements.rs` by role — that file is the rows a commit writes,
//! this is the two questions asked of them, and they share nothing but the
//! table. Both answers are reconstructions rather than lookups, and they
//! reconstruct different things: a total is arithmetic over every row, and a
//! movement is one *fact* recovered from the one or two rows that recorded its
//! effect on this address.
//!
//! **A same-address transfer is two rows and one movement.** Its credit and its
//! debit share every column but `direction`, and both are stored so the pair
//! nets to zero without either side being lost; a page must still show the one
//! movement the chain made. So the reassembly folds them back together, and the
//! fold counts them both.

use std::collections::BTreeMap;

use base::MovementKind;
use num_bigint::BigUint;
use rusqlite::{Row, Transaction, params};

use super::{
    Tables, failure,
    movements::{CREDIT, FEE, Totals, WITHDRAWAL},
    store,
    tables::Place,
};
use crate::{IndexError, MovementId, ValueMovement};

impl Tables {
    /// Every credit and debit of one subject, summed.
    ///
    /// **One statement, streamed, and no cursor.** The rows are read through a
    /// running statement inside the caller's own transaction, so this costs one
    /// row at a time whatever the subject's history is, and there is no resume
    /// position to get wrong. The paginated form this replaces keyed its cursor
    /// on `(height, transaction, movement)` and therefore could not resume
    /// inside a same-address transfer, whose credit and debit share every one of
    /// those columns and differ only in `direction` — a page boundary landing
    /// between them dropped the debit and overstated the subject.
    ///
    /// The arithmetic stays in Rust rather than becoming `SUM()`: an amount here
    /// is u256-capable canonical text and SQLite's only integer is a signed
    /// 64-bit one.
    pub(super) fn totals(
        &self,
        read: &Transaction<'_>,
        subject: (&str, &str),
    ) -> Result<Totals, IndexError> {
        let (address, asset) = subject;
        let mut statement = read
            .prepare_cached(
                "SELECT direction, amount FROM balance_movement
                 WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4",
            )
            .map_err(|error| failure(&error, "failed to prepare a balance fold"))?;
        let mut rows = statement
            .query(params![self.chain(), self.name(), address, asset])
            .map_err(|error| failure(&error, "failed to fold balance movements"))?;
        let mut totals = (BigUint::ZERO, BigUint::ZERO);
        while let Some(row) = Self::step(&mut rows)? {
            let units = Self::decimal(row, 1)?
                .to_atomic(0)
                .map_err(|_| store("a stored amount is not an atomic integer"))?;
            if Self::column::<i64>(row, 0)? == CREDIT {
                totals.0 += units;
            } else {
                totals.1 += units;
            }
        }
        Ok(totals)
    }

    /// Every movement of `address` between the two places, inclusive, grouped by
    /// the transaction it belongs to and in stored order.
    ///
    /// The rows are effects and a [`ValueMovement`] is a fact, so the two rows a
    /// same-address transfer wrote are folded back into the one movement they
    /// describe. The fee and the withdrawal are skipped: the first is already
    /// `transaction_fact`'s three columns and would otherwise appear on a page
    /// twice, and the second belongs to no transaction at all.
    pub(super) fn attached(
        &self,
        read: &Transaction<'_>,
        address: &str,
        asset: Option<&str>,
        bounds: (&Place, &Place),
    ) -> Result<BTreeMap<String, Vec<ValueMovement>>, IndexError> {
        let (lower, upper) = bounds;
        let mut statement = read
            .prepare_cached(
                "SELECT transaction_id, movement_id, direction, kind, asset, amount,
                        counterparty, movement_order
                 FROM balance_movement
                 WHERE chain = ?1 AND network = ?2 AND address = ?3
                   AND block_height >= ?4 AND block_height <= ?5
                   AND transaction_id <> ''
                   AND (?6 IS NULL OR asset = ?6)
                 ORDER BY block_height, transaction_id, movement_order, movement_id",
            )
            .map_err(|error| failure(&error, "failed to prepare a movement read"))?;
        let mut rows = statement
            .query(params![
                self.chain(),
                self.name(),
                address,
                lower.height,
                upper.height,
                asset
            ])
            .map_err(|error| failure(&error, "failed to read movements"))?;
        let mut grouped: BTreeMap<String, Vec<ValueMovement>> = BTreeMap::new();
        let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
        while let Some(row) = Self::step(&mut rows)? {
            self.regroup(row, address, &mut grouped, &mut seen)?;
        }
        Ok(grouped)
    }

    /// Places one row under the transaction it belongs to, merging it into the
    /// movement a row of the other direction already produced.
    fn regroup(
        &self,
        row: &Row<'_>,
        address: &str,
        grouped: &mut BTreeMap<String, Vec<ValueMovement>>,
        seen: &mut BTreeMap<(String, String), usize>,
    ) -> Result<(), IndexError> {
        let Some(kind) = Self::kind(Self::column(row, 3)?)? else {
            return Ok(());
        };
        let identity: String = Self::column(row, 0)?;
        let movement: String = Self::column(row, 1)?;
        let credit = Self::column::<i64>(row, 2)? == CREDIT;
        let counterparty: Option<String> = Self::column(row, 6)?;
        let mine = || Some(self.address(address.to_owned()));
        let other = || counterparty.clone().map(|value| self.address(value));
        let (from, to) = if credit {
            (other(), mine())
        } else {
            (mine(), other())
        };
        let entry = grouped.entry(identity.clone()).or_default();
        if let Some(at) = seen.get(&(identity.clone(), movement.clone())) {
            // The other direction of the same movement: both endpoints are this
            // address, so the pair describes one self-transfer.
            entry[*at] = ValueMovement::stored(
                MovementId(movement),
                self.asset(Self::column(row, 4)?),
                Self::decimal(row, 5)?,
                kind,
                mine(),
                mine(),
            )?;
            return Ok(());
        }
        seen.insert((identity, movement.clone()), entry.len());
        entry.push(ValueMovement::stored(
            MovementId(movement),
            self.asset(Self::column(row, 4)?),
            Self::decimal(row, 5)?,
            kind,
            from,
            to,
        )?);
        Ok(())
    }

    /// The movement kind one stored tag names, or `None` for the two effects
    /// that are balance changes without being movements.
    fn kind(tag: i64) -> Result<Option<MovementKind>, IndexError> {
        match tag {
            0 => Ok(Some(MovementKind::Transfer)),
            1 => Ok(Some(MovementKind::Input)),
            2 => Ok(Some(MovementKind::Output)),
            3 => Ok(Some(MovementKind::Mint)),
            4 => Ok(Some(MovementKind::Burn)),
            FEE | WITHDRAWAL => Ok(None),
            _ => Err(store("a stored movement has an unknown kind")),
        }
    }
}
