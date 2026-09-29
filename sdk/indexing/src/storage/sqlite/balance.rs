//! The balance fold, over one holding's stored effects.
//!
//! Not a `SUM()`, and it never becomes one. An amount here is u256-capable and
//! stored as canonical base-10 text, while SQLite's only integer is a signed
//! 64-bit one (`docs/design/030 §8 (ex-031)` section 1.4), so the arithmetic is
//! [`Ledger`]'s and stays in Rust -- including the overdraw branch, which is the
//! index reporting that it is missing credits rather than an error to swallow.
//!
//! **One collection now, and no chain rule.** The fold used to walk every
//! history row of the address, project a sign onto each movement, decide from a
//! `FeeSettlement` the caller supplied whether the network fee counted, and then
//! walk a second table for the credits no transaction named. All four of those
//! are stored facts now: a `balance_movement` row is one address's balance
//! change in one asset, already signed, with the fee among them exactly on the
//! chains that charge one and the transactionless credits among them too.
//!
//! What the fold is, and must stay, is a **recomputation**, and it is the only
//! one: no running total is maintained anywhere, so there is no second number to
//! keep equal to this one. A recomputation is also the only thing that can prove
//! itself short, which is the sole producer [`crate::Completeness::Overdrawn`]
//! has.

use rusqlite::{Connection, Transaction, params};

use super::{Repository, Tables, failure};
use crate::{
    Balance, BalanceQuery, BalanceStore, BlockPosition, BlockRef, BoxFuture, Completeness, Decimal,
    Effect, Holder, IndexError, Ledger,
};

/// The six progress columns of a `watch` row, which [`Tables::coordinates`]
/// reads as one block reference.
const PROGRESS: &str = "indexed_position, indexed_height, indexed_hash, \
     indexed_parent_position, indexed_parent_hash, indexed_timestamp";

impl BalanceStore for Repository {
    /// Reads the checkpoint and folds the holding inside **one** deferred
    /// transaction, so the balance is one snapshot rather than a fold that
    /// agreed with a checkpoint afterwards.
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            query.validate()?;
            let tables = self.tables(&query.address.network)?;
            self.database
                .blocking(move |connection| tables.folded(connection, &query))
                .await
        })
    }
}

impl Tables {
    /// The subject's settled total, recomputed from its own stored effects, and
    /// what the observation of it — if there is one — can claim about the
    /// history behind that number.
    ///
    /// **One fold, and only one.** Every subject is summed the same way, whether
    /// anything observes it or not. A `watch` row used to carry a second copy of
    /// this number, maintained forward from what each commit's inserts reported
    /// as new, so that reading an observed holding was one row rather than a walk
    /// — two answers to one question, kept equal by a seed at registration and a
    /// delta at every commit, and by a contract test asserting they had not
    /// drifted. The walk they were avoiding is an index range scan over this
    /// table's own leading key in a `WITHOUT ROWID` B-tree, so the second answer
    /// bought little and had to be right forever.
    pub(super) fn folded(
        &self,
        connection: &Connection,
        query: &BalanceQuery,
    ) -> Result<Balance, IndexError> {
        let (read, checkpoint) = self.snapshot(connection)?;
        let subject = (query.address.value.as_str(), query.asset.asset.as_str());
        let totals = self.totals(&read, subject)?;
        let mut ledger = Ledger::new(query);
        ledger.record(&Effect::Credit(Decimal::from_atomic(totals.0, 0)))?;
        ledger.record(&Effect::Debit(Decimal::from_atomic(totals.1, 0)))?;
        let mut balance = ledger.balance(checkpoint);
        // Provenance only ever *weakens* the answer the arithmetic produced, and
        // never the other way round: `Overdrawn` is a proof the rows carry and a
        // claim about provenance cannot talk it away.
        if balance.completeness != Completeness::Overdrawn {
            balance.completeness = self.provenance(&read, subject)?;
        }
        Ok(balance)
    }

    /// What the observation of `subject` can claim about the history behind its
    /// total, or [`Completeness::Incomplete`] when nothing observes it.
    ///
    /// **Answered once for both reads.** A balance and a history page are two
    /// projections of one address's stored rows, so what the store can prove
    /// about the range behind them is one fact, read here inside whichever
    /// read's own snapshot asked for it (`Tables::page` is the other caller).
    /// Two copies of this rule could disagree, and a caller comparing a
    /// complete-looking page against an incomplete-looking total would have no
    /// way to tell which of them was lying.
    ///
    /// **Computed, never stored.** A whole-history claim is a conjunction of two
    /// facts the `watch` row already carries: observation began at the chain's
    /// first block, **and** every block up to the line the answer is read at has
    /// been inspected for it. Dropping the second half is how a fresh row with no
    /// progress at all came to read as authoritative — a birthday says where
    /// inspection would start, not that any of it happened.
    ///
    /// The conjunction is `WatchProgress::caught_up`, the same comparison the
    /// pass and the index's own read gate ask, so a store and an index can never
    /// disagree about what "still catching up" means.
    ///
    /// [`Completeness::VerifiedOpening`] is not reachable from here and there is
    /// no column for it. It is an attestation — an independent read of the same
    /// holding at the same block agreeing with this fold — and the only thing in
    /// this SDK that produces one is `chain_ethereum::BalanceWitness`, which
    /// reads a node and mutates a [`Balance`] on its way to a caller. A column
    /// nothing can write is dead schema, and it costs nothing to add beside the
    /// writer that needs it (`docs/design/034` steps 9-12).
    pub(super) fn provenance(
        &self,
        read: &Transaction<'_>,
        subject: (&str, &str),
    ) -> Result<Completeness, IndexError> {
        let (address, asset) = subject;
        let mut statement = read
            .prepare_cached(&format!(
                "SELECT birthday, {PROGRESS} FROM watch
                 WHERE chain = ?1 AND network = ?2 AND asset = ?3 AND address = ?4"
            ))
            .map_err(|error| failure(&error, "failed to prepare an observation read"))?;
        let mut rows = statement
            .query(params![self.chain(), self.name(), asset, address])
            .map_err(|error| failure(&error, "failed to read an observation"))?;
        let Some(row) = Self::step(&mut rows)? else {
            return Ok(Completeness::Incomplete);
        };
        let birthday = BlockPosition(Self::restored(Self::column(row, 0)?)?);
        let indexed_through = match Self::column::<Option<i64>>(row, 1)? {
            None => None,
            Some(_) => Some(Self::block(Self::coordinates(row, 1).map_err(
                |error| failure(&error, "failed to read recorded progress"),
            )?)?),
        };
        Ok(Self::claimed(birthday, indexed_through.as_ref()))
    }

    /// Whether a row observed from `birthday` and inspected through
    /// `indexed_through` holds the address's whole history.
    fn claimed(birthday: BlockPosition, indexed_through: Option<&BlockRef>) -> Completeness {
        let inspected = indexed_through.is_some_and(|block| block.position >= birthday);
        if birthday == BlockPosition(0) && inspected {
            return Completeness::CompleteHistory;
        }
        Completeness::Incomplete
    }
}
