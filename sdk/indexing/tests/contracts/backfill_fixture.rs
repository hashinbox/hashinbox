//! Shared setup for backfill contracts: observations, canonical blocks and reads.
//! The scenarios remain beside this fixture in backfill.rs.

use base::Decimal;

use indexing::{
    AssetId, BalanceQuery, BalanceStore, BlockCredit, BlockHeight, BlockPosition, BlockRef,
    CanonicalBlock, ConfirmedHistory, FeeSettlement, IndexedBatch, IndexedOutput, InterpretedBlock,
    MovementId, ObservationDraft, ObservationDraftStatus, OutputChanges, OutputId, OutputKey,
    OutputStore, Progress, Repository, ValueMovement, Watch,
};

use super::backfill::BackfillContract;

impl<R: BalanceStore + OutputStore + Repository + ConfirmedHistory> BackfillContract<'_, R> {
    /// Commits one batch of `[from, to]` under the current checkpoint, expecting
    /// whatever progress the store actually holds.
    pub(super) async fn filled(&self, from: BlockPosition, to: BlockPosition) {
        let checkpoint = self.checkpoint().await;
        let expected = self.progress().await;
        let batch = self.batch(from, to, checkpoint, expected);
        self.fixture
            .repository
            .commit(batch)
            .await
            .expect("one batch below the line");
    }

    /// One batch of `[from, to]` planned under `checkpoint`, advancing the
    /// fixture's observation to `to` from `expected`.
    pub(super) fn batch(
        &self,
        from: BlockPosition,
        to: BlockPosition,
        checkpoint: BlockRef,
        expected: Option<BlockRef>,
    ) -> IndexedBatch {
        let blocks = (from.0..=to.0)
            .map(|height| self.canonical(height))
            .collect();
        let progress = vec![Progress {
            watch_id: self.watch().id,
            expected,
            indexed_through: self.fixture.block(to.0),
        }];
        IndexedBatch::new(
            self.fixture.network.clone(),
            Some(checkpoint),
            blocks,
            progress,
            FeeSettlement::Charged,
        )
        .expect("a valid batch")
    }

    /// Commits `[from, to]` with **no claim**: rows in the store, and nothing
    /// said about how far the observation got.
    ///
    /// What a scenario uses when what it is about is the rows or the output fold
    /// rather than the progress — including one that re-reads a *shorter* range
    /// than a previous batch already claimed, which is not a claim this index
    /// would ever make and which the batch constructor refuses as a retreat.
    pub(super) async fn seed(&self, from: u64, to: u64) {
        let blocks = (from..=to).map(|height| self.canonical(height)).collect();
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            Some(self.checkpoint().await),
            blocks,
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch below the line");
        self.fixture
            .repository
            .commit(batch)
            .await
            .expect("one batch below the line");
    }

    /// The block at `height` as it is *not* stored: same transaction, no fee, so
    /// one `(address, height, transaction)` would have two contents.
    pub(super) fn disagreeing(&self, height: u64) -> CanonicalBlock {
        self.canonical_of(
            height,
            vec![ObservationDraft {
                fee: None,
                ..self.draft(height)
            }],
            OutputChanges::default(),
            Vec::new(),
        )
    }

    /// The fixture's five blocks, committed with nothing watched.
    pub(super) async fn prepare(&self) {
        self.fixture
            .repository
            .add(vec![self.watch()])
            .await
            .expect("the observation whose past this fills in");
        let mut checkpoint = None;
        for height in 1..=5 {
            let block = self.fixture.block(height);
            let canonical =
                self.canonical_of(height, Vec::new(), OutputChanges::default(), Vec::new());
            let batch = IndexedBatch::new(
                self.fixture.network.clone(),
                checkpoint,
                vec![canonical],
                Vec::new(),
                FeeSettlement::Charged,
            )
            .expect("a valid fixture block");
            self.fixture
                .repository
                .commit(batch)
                .await
                .expect("fixture block");
            checkpoint = Some(block);
        }
    }

    /// One block as the interpreter reports it for the pending address, already
    /// validated.
    pub(super) fn canonical(&self, height: u64) -> CanonicalBlock {
        let created = (height == 3).then(|| self.output()).into_iter().collect();
        let spent = if height == 5 {
            vec![self.output().key()]
        } else {
            Vec::new()
        };
        self.canonical_of(
            height,
            vec![self.draft(height)],
            OutputChanges {
                created,
                spent,
                tracked_spends: Vec::new(),
            },
            (height == 4)
                .then(|| BlockCredit {
                    id: MovementId("withdrawal:4".into()),
                    asset: self.asset(),
                    amount: Decimal::from(7_u64),
                    to: self.fixture.address("late"),
                })
                .into_iter()
                .collect(),
        )
    }

    /// Validated facts for block `height` with exactly the content given.
    pub(super) fn canonical_of(
        &self,
        height: u64,
        transactions: Vec<ObservationDraft>,
        outputs: OutputChanges,
        credits: Vec<BlockCredit>,
    ) -> CanonicalBlock {
        CanonicalBlock::new(
            self.fixture.network.clone(),
            InterpretedBlock {
                outputs,
                credits,
                ..InterpretedBlock::new(
                    &self.fixture.network,
                    self.fixture.block(height),
                    transactions,
                )
                .expect("unique transaction ids")
            },
        )
        .expect("valid fixture facts")
    }

    /// The settled balance of `late`, under an account chain's fee rule.
    pub(super) async fn settled(&self) -> Decimal {
        self.fixture
            .repository
            .balance(BalanceQuery {
                address: self.fixture.address("late"),
                asset: self.asset(),
            })
            .await
            .expect("the folded balance")
            .amount
    }

    pub(super) fn draft(&self, height: u64) -> ObservationDraft {
        ObservationDraft {
            transaction_id: self.fixture.transaction(&format!("block-{height}")),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId(format!("m-{height}")),
                self.asset(),
                Decimal::from(10_u64),
                self.fixture.address("payer"),
                self.fixture.address("late"),
            )],
            fee: Some(indexing::NetworkFee {
                asset: self.asset(),
                amount: Decimal::from(1_u64),
                payer: Some(self.fixture.address("payer")),
            }),
        }
    }

    pub(super) fn output(&self) -> IndexedOutput {
        IndexedOutput {
            id: OutputId {
                transaction: self.fixture.transaction("block-3"),
                index: 0,
            },
            address: self.fixture.address("late"),
            asset: self.asset(),
            amount: Decimal::from(10_u64),
            evidence: vec![1],
            created_at: BlockHeight(3),
            coinbase: false,
        }
    }

    pub(super) async fn history(&self, address: &str) -> Vec<String> {
        self.fixture.history(address, &self.asset(), 50).await
    }

    /// The live output rows of `late`, looked at while nothing observes it.
    ///
    /// A store refuses the spendable set of an address that is still being
    /// inspected below the line, so a scenario about the *rows* a partial read
    /// left behind cannot list them as a wallet would. It withdraws the
    /// observation, which keeps every row, reads them as the rows of an address
    /// nothing is waiting on, and registers the observation again.
    ///
    /// Only for a state that holds no claim: withdrawing discards one, and a
    /// scenario that silently lost its progress here would be asserting about a
    /// different state than the one it built.
    pub(super) async fn rows(&self) -> Vec<OutputKey> {
        assert_eq!(
            self.progress().await,
            None,
            "withdrawing the observation would discard the claim under test"
        );
        let repository = self.fixture.repository;
        repository
            .forget(&self.fixture.network, &self.watch())
            .await
            .expect("the observation withdrawn");
        let rows = self.outputs().await;
        repository
            .add(vec![self.watch()])
            .await
            .expect("the observation registered again");
        rows
    }

    /// The spendable set of `late`, as a wallet is offered it: refused while
    /// the address is still being inspected below the line.
    pub(super) async fn outputs(&self) -> Vec<OutputKey> {
        self.fixture
            .outputs("late", 50)
            .await
            .iter()
            .map(IndexedOutput::key)
            .collect()
    }

    /// The progress the store recorded for the fixture's observation, read the
    /// only way the port offers it.
    pub(super) async fn progress(&self) -> Option<BlockRef> {
        self.fixture
            .repository
            .all(&self.fixture.network)
            .await
            .expect("observations")
            .into_iter()
            .find(|observed| observed.watch.id == self.watch().id)
            .and_then(|observed| observed.indexed_through)
    }

    pub(super) fn asset(&self) -> AssetId {
        self.fixture.native()
    }

    /// The observation whose past the fixture fills in.
    pub(super) fn watch(&self) -> Watch {
        Watch {
            id: format!(
                "{}-{}-late",
                self.fixture.network.chain, self.fixture.network.name
            ),
            address: self.fixture.address("late"),
            asset: self.asset(),
            start: BlockPosition(1),
        }
    }

    pub(super) async fn checkpoint(&self) -> BlockRef {
        self.fixture
            .repository
            .checkpoint(&self.fixture.network)
            .await
            .expect("checkpoint")
            .expect("the fixture committed blocks")
    }
}
