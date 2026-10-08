//! What a block presented a second time may not do.
//!
//! Split from `unmatched_spends.rs` by role, as `rollback.rs` and
//! `backfill_fixture.rs` are from theirs: that file states what one batch's
//! spends report and what the live set looks like afterwards, and this states
//! the one property those assertions cannot see.
//!
//! **The live set was never the property at risk.** A batch used to be folded
//! before it reached a store: a spend naming an output the batch itself created
//! cancelled that creation, and both were dropped on the grounds that an output
//! born and spent inside one batch "never exists". That is true of the live set
//! — which is exactly why every scenario in `unmatched_spends.rs` passed — and false of
//! the *record*. Dropping the spend threw away the only fact that makes
//! re-presenting the creating block safe, so a historical read of that block put
//! the output back as spendable: the defect a durable record of unmatched spends
//! had been introduced to prevent, reintroduced by the fold meant to make it
//! unnecessary.
//!
//! So the property is stated about the replay rather than about the live set,
//! and it is stated for the case the fold hid: **a creation and its spend inside
//! one batch**. A store reaches the same live set by applying each block's
//! creations and then its spends, in block order, inside one transaction — and
//! keeps the spend while doing it.

use indexing::{
    BlockHeight, BlockPosition, BlockRef, BlockWithOutputs, FeeSettlement, IndexStore,
    IndexedBatch, OutputChanges, OutputKey, OutputStore, Repository,
};

use super::{backfill::BackfillContract, unmatched_spends::SpendContract};

impl<R: IndexStore + OutputStore> SpendContract<'_, R> {
    /// An output created and spent inside one batch stays spent when the block
    /// that created it is presented again.
    ///
    /// The second presentation is a historical read: block 1 is below the line
    /// the first batch left behind, which is exactly where a backfill inspecting
    /// it for a newly watched address arrives from. It is made twice, because a
    /// batch whose outcome was lost is redone and the redo must be worth as
    /// little as the first repeat.
    pub async fn a_replayed_creating_block_never_resurrects_a_spent_output(&self) {
        let funding = self.fixture.output("owner", "funding", BlockHeight(1));
        let created = self.block(
            1,
            OutputChanges {
                created: vec![funding.clone()],
                ..OutputChanges::default()
            },
        );
        let spent = self.block(
            2,
            OutputChanges {
                spent: vec![funding.key()],
                ..OutputChanges::default()
            },
        );

        let outcome = self.commit(None, vec![created.clone(), spent]).await;
        assert!(
            outcome.is_empty(),
            "the spend matched the creation the same batch carried, so nothing \
             is reported missing",
        );
        assert!(
            self.outputs("owner").await.is_empty(),
            "an output created and spent inside one batch is not spendable",
        );

        for redone in [false, true] {
            let replayed = self
                .commit(Some(self.fixture.block(2)), vec![created.clone()])
                .await;
            assert!(
                replayed.is_empty(),
                "a replay of the creating block spends nothing (redone: {redone})",
            );
            assert!(
                self.outputs("owner").await.is_empty(),
                "re-presenting the block that created a spent output must not \
                 offer it again (redone: {redone})",
            );
        }
    }

    /// Block `height` of the fixture chain, carrying `outputs` and the one
    /// transaction every fixture block carries.
    fn block(&self, height: u64, outputs: OutputChanges) -> BlockWithOutputs {
        let block = self.fixture.block(height);
        let transactions = vec![self.fixture.paid("owner", block.height)];
        BlockWithOutputs {
            outputs,
            ..BlockWithOutputs::new(&self.fixture.network, block, transactions)
                .expect("unique transaction ids")
        }
    }

    /// One batch of `blocks` above `checkpoint`, and the spends it could not
    /// account for.
    async fn commit(
        &self,
        checkpoint: Option<BlockRef>,
        blocks: Vec<BlockWithOutputs>,
    ) -> Vec<OutputKey> {
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            checkpoint,
            blocks,
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch");
        self.fixture
            .repository
            .commit(batch)
            .await
            .expect("the batch commits")
    }
}

impl<R: OutputStore + Repository> BackfillContract<'_, R> {
    /// A historical read that stops short of a spend does not put the spent
    /// output back.
    ///
    /// Three statements in one fixture, because a store can only be wrong about
    /// this in one of two directions and a one-sided assertion cannot tell a
    /// working rule from a store that suppresses every historical creation:
    ///
    /// 1. A read covering the creation and nothing that spends it leaves the
    ///    output live. That is the ordinary backfill.
    /// 2. A read covering the whole of the output's life leaves it unspendable.
    /// 3. **Reading the first range again afterwards still leaves it
    ///    unspendable.** The spend is above where any later chunk reaches, so no
    ///    chunk will ever carry it again; a store that answered this one by
    ///    re-creating the output would offer a coin nothing can spend, for good.
    ///
    /// The third used to assert the opposite, on the strength of a fold that
    /// dropped a creation and its later spend together. It passed because the
    /// live set was right at the moment the batch landed, and the fact it threw
    /// away — that the output had been spent at all — was the one a replay
    /// needed.
    pub async fn leaves_no_output_the_batch_itself_spent(&self) {
        self.prepare().await;

        self.seed(1, 4).await;
        assert_eq!(
            self.rows().await.len(),
            1,
            "a read that covers the creation and no spend of it leaves the \
             output live",
        );

        self.filled(BlockPosition(1), BlockPosition(5)).await;
        assert!(
            self.outputs().await.is_empty(),
            "the range covers the output's whole life; it is not spendable",
        );

        self.seed(1, 4).await;
        assert!(
            self.outputs().await.is_empty(),
            "and reading the shorter range again does not resurrect it",
        );
    }
}
