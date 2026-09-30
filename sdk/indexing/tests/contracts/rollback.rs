//! What a *refused* batch leaves behind, over the fixture `backfill.rs` and
//! `backfill_fixture.rs` already build.
//!
//! Split by role for the reason `backfill_fixture.rs` states — one file per role rather
//! than one per type, under the repository's 500-line cap. `backfill.rs` holds
//! the guarantees a batch that lands owes; this holds the one a batch that is
//! refused owes, and it is a separate guarantee because **a history page is not
//! the whole of what a commit writes**. The rows a balance is folded from
//! include the credits no transaction names, and the live-output set is a third
//! collection again, so a store whose rollback covered its history rows alone
//! would satisfy every refusal scenario beside this one and still move a
//! holding's settled total on a write that was refused.
//!
//! It is stated against the settled *balance* rather than against a credit row,
//! because the balance is the value a consumer reads and the one a persisted
//! total would have to keep: the assertion survives the change from a fold to a
//! stored number, where an assertion about a table would not.

use base::Decimal;

use indexing::{
    BalanceStore, BlockPosition, ConfirmedHistory, IndexErrorKind, OutputStore, Repository,
};

use super::backfill::BackfillContract;

impl<R: BalanceStore + OutputStore + Repository + ConfirmedHistory> BackfillContract<'_, R> {
    /// A refused batch moves neither the settled balance nor the live-output
    /// set, and the batch that replaces it moves both.
    ///
    /// The second half is what makes the first one a rollback rather than a
    /// batch that would have changed nothing anyway: blocks 4 and 5 carry the
    /// credit and the spend, so both the balance and the output set have to move
    /// once the batch is accepted. Under the refusal every kind of row a commit
    /// writes — the movements, the credit a balance folds, the live output, and
    /// the claim — is exactly where the accepted batch left it.
    pub async fn refuses_a_batch_without_moving_the_balance_or_the_outputs(&self) {
        self.prepare().await;
        self.seed(1, 3).await;
        let live = self.rows().await;
        assert_eq!(live.len(), 1, "block 3 created one output, unspent so far");
        assert_eq!(
            self.settled().await,
            Decimal::from(30_u64),
            "three blocks paying 10 each, and nothing has credited it yet"
        );

        // Blocks 4 and 5 — the credit and the spend — under a claim that says
        // the store holds progress through block 3, which it never recorded.
        let stale = self.batch(
            BlockPosition(4),
            BlockPosition(5),
            self.checkpoint().await,
            Some(self.fixture.block(3)),
        );
        let error = self
            .fixture
            .repository
            .commit(stale)
            .await
            .expect_err("a claim that expected other progress");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(
            self.settled().await,
            Decimal::from(30_u64),
            "the credit block 4 carried did not land, so the settled total is \
             where the accepted batch left it"
        );
        assert_eq!(
            self.rows().await,
            live,
            "nor did the spend block 5 carried, so the output is still live"
        );
        assert_eq!(self.progress().await, None, "nor the claim it made");

        self.filled(BlockPosition(4), BlockPosition(5)).await;

        assert_eq!(
            self.settled().await,
            Decimal::from(57_u64),
            "five blocks paying 10 each, plus the 7 block 4 credited"
        );
        assert!(self.outputs().await.is_empty(), "and the spend removed it");
        assert_eq!(self.progress().await, Some(self.fixture.block(5)));
    }
}
