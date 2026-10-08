//! The required-spend contract.

use indexing::{
    AssetId, BlockHeight, BlockRef, BlockWithOutputs, FeeSettlement, IndexErrorKind, IndexStore,
    IndexedBatch, IndexedOutput, Network, OutputChanges, OutputCursor, OutputId, OutputKey,
    OutputPage, OutputRequest, OutputStore,
};

use super::fixture::Fixture;

/// A spend recorded before its creation prevents historical insertion resurrecting it.
pub struct SpendContract<'a, R> {
    pub(super) fixture: Fixture<'a, R>,
}

impl<'a, R: IndexStore + OutputStore> SpendContract<'a, R> {
    /// Checks `repository`, which must hold nothing for `network` yet.
    #[must_use]
    pub const fn new(repository: &'a R, network: Network) -> Self {
        Self {
            fixture: Fixture::new(repository, network),
        }
    }

    /// Reads the store's own rows with `rows` where the scenario checks them.
    #[must_use]
    pub fn reading(mut self, rows: super::Rows) -> Self {
        self.fixture.rows = Some(rows);
        self
    }

    /// Spending an output not yet indexed still prevents its later resurrection.
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
        assert!(
            self.history("owner")
                .await
                .is_none_or(|history| history == ["block-1"])
        );
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
        assert!(
            self.history("owner")
                .await
                .is_none_or(|history| history == ["block-1", "block-2"])
        );
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

    /// A page of live outputs is as long as it was asked to be, and only a page
    /// with an output after it hands out a cursor.
    ///
    /// Both halves matter to whoever pages the spendable set. A last page that
    /// still named a successor sends coin selection round again for nothing,
    /// and a full page that named none hides every output behind it. The two
    /// stores fetch their rows differently and must end the listing alike.
    pub async fn an_output_page_names_a_successor_only_when_one_exists(&self) {
        let created = ["one", "two", "three"]
            .map(|id| self.fixture.output("owner", id, BlockHeight(1)))
            .to_vec();
        let changes = OutputChanges {
            created: created.clone(),
            ..OutputChanges::default()
        };
        self.add(self.fixture.block(1), None, changes)
            .await
            .expect("funding block");

        let opening = self.page(None, 2).await;
        assert_eq!(opening.outputs.len(), 2, "a full page");
        let cursor = opening.next.expect("a third output follows");
        let closing = self.page(Some(cursor), 2).await;
        assert_eq!(closing.next, None, "nothing follows the last output");
        let mut listed = [opening.outputs, closing.outputs].concat();
        let mut expected = created;
        listed.sort_by_key(|output| output.id.transaction.id.to_string());
        expected.sort_by_key(|output| output.id.transaction.id.to_string());
        assert_eq!(listed, expected, "every output, each on one page");

        let exact = self.page(None, 3).await;
        assert_eq!(exact.outputs.len(), 3);
        assert_eq!(exact.next, None, "a full page is not thereby followed");
    }

    /// One page of up to `limit` of the owner's live outputs, after `after`.
    async fn page(&self, after: Option<OutputCursor>, limit: usize) -> OutputPage {
        let request = OutputRequest {
            after,
            ..self.fixture.request("owner", limit)
        };
        OutputStore::list(self.fixture.repository, request)
            .await
            .expect("a page of live outputs")
    }

    /// One batch below `checkpoint`, carrying the block that created `output`.
    ///
    /// The same [`IndexStore::commit`] a forward block goes through: the block is
    /// at or below the line the batch names, so the line does not move.
    async fn fill(&self, checkpoint: &BlockRef, output: &IndexedOutput) {
        let block = self.fixture.block(output.created_at.0);
        let transactions = vec![self.fixture.paid("owner", block.height)];
        let interpreted = BlockWithOutputs {
            outputs: OutputChanges {
                created: vec![output.clone()],
                ..OutputChanges::default()
            },
            ..BlockWithOutputs::new(&self.fixture.network, block, transactions)
                .expect("unique transaction ids")
        };
        let canonical = interpreted;
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            Some(checkpoint.clone()),
            vec![canonical],
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
        let interpreted = BlockWithOutputs {
            outputs,
            ..BlockWithOutputs::new(
                &self.fixture.network,
                block,
                vec![self.fixture.paid("owner", height)],
            )
            .expect("unique transaction ids")
        };
        let canonical = interpreted;
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            checkpoint,
            vec![canonical],
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

    async fn history(&self, owner: &str) -> Option<Vec<String>> {
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
