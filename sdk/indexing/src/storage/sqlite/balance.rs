//! The balance fold, over one holding's stored effects.
//!
//! Not a `SUM()`, and it never becomes one. An amount here is u256-capable and
//! stored as canonical base-10 text, while SQLite's only integer is a signed
//! 64-bit one (`docs/design/030 §8 (ex-031)` section 1.4), so the arithmetic is
//! [`Fold`]'s and stays in Rust -- including the overdraw branch, which is the
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

use rusqlite::{Connection, Transaction};

use super::{Repository, Tables};
use crate::{
    Balance, BalanceQuery, BalanceStore, BlockRef, BoxFuture, Completeness, Decimal, Fold, Holder,
    IndexError, Inspection, MovementEffect,
};

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
        let mut ledger = Fold::new(query);
        ledger.record(&MovementEffect::Credit(Decimal::from_atomic(totals.0, 0)))?;
        ledger.record(&MovementEffect::Debit(Decimal::from_atomic(totals.1, 0)))?;
        let mut balance = ledger.balance(checkpoint);
        // Provenance only ever *weakens* the answer the arithmetic produced, and
        // never the other way round: `Overdrawn` is a proof the rows carry and a
        // claim about provenance cannot talk it away.
        if balance.completeness != Completeness::Overdrawn {
            balance.completeness = self.provenance(&read, query, balance.observed_at.as_ref())?;
        }
        Ok(balance)
    }

    /// What the observations of `of`'s address can claim about the history
    /// behind its rows, at the `checkpoint` of the snapshot `read` holds.
    ///
    /// **Answered once for both reads.** A balance and a history page are two
    /// projections of one address's stored rows, so what the store can prove
    /// about the range behind them is one fact, read here inside whichever
    /// read's own snapshot asked for it (`Tables::page` is the other caller).
    /// Two copies of this rule could disagree, and a caller comparing a
    /// complete-looking page against an incomplete-looking total would have no
    /// way to tell which of them was lying.
    ///
    /// **Computed, never stored.** A whole-history claim is a conjunction of
    /// facts the `watch` rows already carry and the line this snapshot was read
    /// at: observation began at the chain's first block, progress has been
    /// recorded for it, **and** no observation of the address is still behind
    /// that line. The rule is [`Inspection`]'s and both stores ask it; what is
    /// this store's own is that the rows it is asked over come from the same
    /// transaction as the answer they qualify, which is why no index has to
    /// repair the claim afterwards.
    pub(super) fn provenance(
        &self,
        read: &Transaction<'_>,
        of: &dyn Holder,
        checkpoint: Option<&BlockRef>,
    ) -> Result<Completeness, IndexError> {
        let observations = self.observing(read, &of.address().value)?;
        Ok(Inspection::new(&observations, checkpoint).completeness(of))
    }
}
