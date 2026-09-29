//! The required-spend contract.

use indexing::{
    AssetId, BlockHeight, BlockRef, CanonicalBlock, ConfirmedHistory, FeeSettlement,
    IndexErrorKind, IndexStore, IndexedBatch, IndexedOutput, InterpretedBlock, Network,
    OutputChanges, OutputId, OutputKey, OutputStore,
};

use super::fixture::Fixture;

/// The rule of `docs/design/024` §5.1, checked against one repository: a
/// required spend the index cannot match is a completeness fact about one
/// address, never a verdict on the block.
///
/// And the rule's second half, which is about the store rather than the block:
/// the miss is *kept*. An address is watched above its own funding, so the
/// output a spend could not find is one a later batch may still discover below
/// the checkpoint — and the spend that named it was applied above where that
/// batch reaches, so no batch ever carries it again. A store that forgot the
/// miss would fill the creation in and leave an output nothing can ever spend.
///
/// Every scenario writes its own fixture over an empty repository bound to
/// `network`, and the two backends must agree on all of it — including which
/// conditions are *still* errors.
pub struct SpendContract<'a, R> {
    pub(super) fixture: Fixture<'a, R>,
}

impl<'a, R: IndexStore + OutputStore + ConfirmedHistory> SpendContract<'a, R> {
    /// Checks `repository`, which must hold nothing for `network` yet.
    #[must_use]
    pub const fn new(repository: &'a R, network: Network) -> Self {
        Self {
            fixture: Fixture::new(repository, network),
        }
    }

    /// The defect of `docs/design/024` §1.3: a block that spends an output this
    /// network never indexed commits, names the key it could not match, and
    /// leaves every other fact of the block in place.
    pub async fn an_unknown_required_spend_is_reported_not_refused(&self) {
        let ghost = self.key("owner", "ghost");
        let fresh = self.fixture.output("owner", "fresh", BlockHeight(1));
        let first = self.fixture.block(1);
        let outcome = self
            .add(
                first.clone(),
                None,
                OutputChanges {
                    created: vec![fresh.clone()],
                    spent: vec![ghost.clone()],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect("an unmatched required spend must not fail the block");

        assert_eq!(outcome.as_slice(), [ghost]);
        assert_eq!(self.tip().await, Some(first));
        assert_eq!(self.outputs("owner").await, [fresh]);
        assert_eq!(self.history("owner").await, ["block-1"]);
    }

    /// The key is address-qualified, so a required spend naming the wrong
    /// owner misses. It is reported, and the real owner keeps its output.
    pub async fn a_required_spend_of_another_owner_is_reported(&self) {
        let funding = self.fixture.output("owner", "funding", BlockHeight(1));
        let first = self.fixture.block(1);
        self.fund(&first, &funding).await;

        let intruder = OutputKey {
            address: self.fixture.address("intruder"),
            output: funding.id.clone(),
        };
        let second = self.fixture.block(2);
        let outcome = self
            .add(
                second.clone(),
                Some(first),
                OutputChanges {
                    spent: vec![intruder.clone()],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect("a wrong-owner required spend must not fail the block");

        assert_eq!(outcome.as_slice(), [intruder]);
        assert_eq!(self.tip().await, Some(second));
        assert_eq!(self.outputs("owner").await, [funding]);
    }

    /// The ordinary path is unchanged: a spend the index can account for
    /// reports nothing and removes the live output.
    pub async fn a_matched_spend_reports_nothing(&self) {
        let funding = self.fixture.output("owner", "funding", BlockHeight(1));
        let first = self.fixture.block(1);
        self.fund(&first, &funding).await;

        let second = self.fixture.block(2);
        let outcome = self
            .add(
                second,
                Some(first),
                OutputChanges {
                    spent: vec![funding.key()],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect("a matched spend commits");

        assert_eq!(outcome.as_slice(), []);
        assert!(self.outputs("owner").await.is_empty());
    }

    /// A tracked spend of an output the index does not hold stays silent: it
    /// is not reported, because no address claimed it.
    pub async fn a_tracked_spend_is_never_reported(&self) {
        let first = self.fixture.block(1);
        let outcome = self
            .add(
                first,
                None,
                OutputChanges {
                    tracked_spends: vec![self.key("owner", "ghost")],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect("a tracked spend of an unindexed output is ordinary");

        assert_eq!(outcome.as_slice(), []);
    }

    /// A reported miss writes nothing for the key it could not match, and
    /// commits everything else the block carries. The store stays exactly one
    /// block ahead: the earlier output is untouched, the new one is live, and
    /// the missing key left no trace at all.
    pub async fn a_reported_miss_commits_the_rest_of_its_block(&self) {
        let funding = self.fixture.output("owner", "funding", BlockHeight(1));
        let first = self.fixture.block(1);
        self.fund(&first, &funding).await;

        let second = self.fixture.block(2);
        let later = self.fixture.output("owner", "later", BlockHeight(2));
        let outcome = self
            .add(
                second.clone(),
                Some(first),
                OutputChanges {
                    created: vec![later.clone()],
                    spent: vec![self.key("owner", "ghost")],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect("the block with the miss commits");
        assert_eq!(outcome.len(), 1);

        assert_eq!(self.tip().await, Some(second));
        let mut live = self.outputs("owner").await;
        live.sort_by_key(|output| output.created_at);
        assert_eq!(live, [funding, later]);
        assert_eq!(self.history("owner").await, ["block-1", "block-2"]);
    }

    /// A miss outlives the block that reported it: an output the index learns
    /// about only *after* it was spent never becomes live.
    ///
    /// The interleaving every running network has. Block 2 spends an output
    /// block 1 created, which nothing has filled in for this address yet, so the
    /// spend matches no row and is reported. The batch that then discovers that
    /// creation must cancel it against the miss rather than insert it: block 2 is
    /// above the address's anchor, the backfill stops at the anchor, and nothing
    /// will ever present that spend again — an inserted output would stay
    /// selectable for good, and coin selection would offer it.
    ///
    /// The batch runs twice, because a batch whose outcome was lost is redone: a
    /// store that consumed the miss to cancel the first create would insert the
    /// output on the second.
    pub async fn a_reported_miss_outlives_the_block_that_reported_it(&self) {
        let funding = self.fixture.output("owner", "funding", BlockHeight(1));
        let first = self.fixture.block(1);
        let second = self.fixture.block(2);
        self.add(first.clone(), None, OutputChanges::default())
            .await
            .expect("the block that created it, indexed for nobody");
        let outcome = self
            .add(
                second.clone(),
                Some(first),
                OutputChanges {
                    spent: vec![funding.key()],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect("the spend commits");
        assert_eq!(outcome.as_slice(), [funding.key()]);

        for redone in [false, true] {
            self.fill(&second, &funding).await;
            assert!(
                self.outputs("owner").await.is_empty(),
                "the chunk filled in an output block 2 had already spent \
                 (redone: {redone})",
            );
        }
    }

    /// The other order needs no record of anything, and must not be spoiled by
    /// one: a chunk that fills the creation in first leaves the output live, and
    /// the next block finds it and spends it like any other.
    ///
    /// Worth pinning beside the scenario above, because a store that suppressed
    /// every historical create of an output *any* spend had ever named would
    /// pass that one and break this one.
    pub async fn a_backfilled_output_is_spent_by_the_next_block(&self) {
        let funding = self.fixture.output("owner", "funding", BlockHeight(1));
        let first = self.fixture.block(1);
        self.add(first.clone(), None, OutputChanges::default())
            .await
            .expect("the block that created it, indexed for nobody");

        self.fill(&first, &funding).await;
        let spend = funding.key();
        assert_eq!(
            self.outputs("owner").await,
            [funding],
            "the chunk filled in an output nothing had spent",
        );

        let outcome = self
            .add(
                self.fixture.block(2),
                Some(first),
                OutputChanges {
                    spent: vec![spend],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect("the spend commits");

        assert_eq!(
            outcome.as_slice(),
            [],
            "the spend found the backfilled output and matched it",
        );
        assert!(
            self.outputs("owner").await.is_empty(),
            "so it removed it, and there was no miss to remember",
        );
    }

    /// One batch below `checkpoint`, carrying the block that created `output`.
    ///
    /// The same [`IndexStore::commit`] a forward block goes through: the block is
    /// at or below the line the batch names, so the line does not move.
    async fn fill(&self, checkpoint: &BlockRef, output: &IndexedOutput) {
        let block = self.fixture.block(output.created_at.0);
        let interpreted = InterpretedBlock {
            transactions: vec![self.fixture.paid("owner", block.height)],
            block,
            outputs: OutputChanges {
                created: vec![output.clone()],
                ..OutputChanges::default()
            },
            credits: Vec::new(),
        };
        let facts = CanonicalBlock::new(self.fixture.network.clone(), interpreted)
            .expect("valid fixture facts");
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            Some(checkpoint.clone()),
            vec![facts],
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

    /// The other meaning of the old error survives: re-creating a live output
    /// under different contents is corruption and still fails, leaving the
    /// checkpoint put.
    ///
    /// Terminal rather than retryable now, and that is the honest kind: one
    /// `(address, outpoint)` cannot have two contents, and reading the block
    /// again cannot change what the file holds. The one rule every block in a
    /// batch is written under — insert-if-absent, verify-if-present — is what
    /// makes an *identical* re-presentation harmless and a disagreeing one fatal.
    pub async fn a_recreated_output_is_still_an_error(&self) {
        let funding = self.fixture.output("owner", "funding", BlockHeight(1));
        let first = self.fixture.block(1);
        self.fund(&first, &funding).await;

        let mut again = funding.clone();
        again.created_at = BlockHeight(2);
        let error = self
            .add(
                self.fixture.block(2),
                Some(first.clone()),
                OutputChanges {
                    created: vec![again],
                    ..OutputChanges::default()
                },
            )
            .await
            .expect_err("re-creating a live output is corruption, not a miss");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "retrying cannot repair the store");
        assert_eq!(self.tip().await, Some(first));
        assert_eq!(self.outputs("owner").await, [funding]);
    }

    async fn fund(&self, block: &BlockRef, output: &IndexedOutput) {
        self.add(
            block.clone(),
            None,
            OutputChanges {
                created: vec![output.clone()],
                ..OutputChanges::default()
            },
        )
        .await
        .expect("funding block");
    }

    async fn add(
        &self,
        block: BlockRef,
        checkpoint: Option<BlockRef>,
        outputs: OutputChanges,
    ) -> Result<Vec<OutputKey>, indexing::IndexError> {
        let height = block.height;
        let interpreted = InterpretedBlock {
            block,
            transactions: vec![self.fixture.paid("owner", height)],
            outputs,
            credits: Vec::new(),
        };
        let facts = CanonicalBlock::new(self.fixture.network.clone(), interpreted)
            .expect("valid fixture facts");
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            checkpoint,
            vec![facts],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("valid fixture block");
        self.fixture.repository.commit(batch).await
    }

    async fn tip(&self) -> Option<BlockRef> {
        self.fixture
            .repository
            .checkpoint(&self.fixture.network)
            .await
            .expect("checkpoint")
    }

    pub(super) async fn outputs(&self, owner: &str) -> Vec<IndexedOutput> {
        self.fixture.outputs(owner, 100).await
    }

    async fn history(&self, owner: &str) -> Vec<String> {
        self.fixture.history(owner, &self.asset(), 100).await
    }

    fn key(&self, owner: &str, transaction: &str) -> OutputKey {
        OutputKey {
            address: self.fixture.address(owner),
            output: OutputId {
                transaction: self.fixture.transaction(transaction),
                index: 0,
            },
        }
    }

    fn asset(&self) -> AssetId {
        self.fixture.native()
    }
}
