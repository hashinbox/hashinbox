//! The contract for the half of a commit that reaches *below* the confirmed
//! line, and for the per-observation progress every commit carries.
//!
//! Stated entirely in operations and guarantees, so it outlives whichever engine
//! answers it. A store passes when, and only when, it gives the pass five
//! things: progress read back beside the observation it belongs to; one batch
//! written all-or-nothing with the claims that describe it; a compare-and-set
//! against the checkpoint that refuses a batch planned under a different one; a
//! compare-and-set against each claim's own expected value; and an idempotent
//! history write that verifies a row it already holds instead of refusing it.

use base::Decimal;

use indexing::{
    BalanceStore, BlockPosition, ConfirmedHistory, FeeSettlement, IndexErrorKind, IndexedBatch,
    Network, OutputStore, Progress, Repository, Watch,
};

use super::fixture::Fixture;

/// The write contract ([`indexing::IndexStore::commit`]) below the line, checked
/// against one repository.
///
/// The fixture commits blocks 1 to 5 with **no address watched**, which is the
/// situation the whole half exists for: every block pays `late` 10 and none of
/// them was indexed for it. Block 3 also creates an output for `late` that
/// block 5 spends, so a batch spanning both must not leave a live output
/// behind, and block 4 credits `late` 7 with no transaction at all — the one
/// change a store that only refilled history would silently drop.
pub struct BackfillContract<'a, R> {
    pub(super) fixture: Fixture<'a, R>,
}

impl<'a, R: BalanceStore + OutputStore + Repository + ConfirmedHistory> BackfillContract<'a, R> {
    /// Checks `repository`, which must hold nothing for `network` yet.
    #[must_use]
    pub const fn new(repository: &'a R, network: Network) -> Self {
        Self {
            fixture: Fixture::new(repository, network),
        }
    }

    /// A spendable set is refused until the address has been inspected up to
    /// the line the page would be read at, and offered the moment it has.
    ///
    /// Blocks 1 to 3 are filled in under a line at 5. The store legitimately
    /// holds the output block 3 created, the page would be stamped with block
    /// 5, and the spend in block 5 has not been looked at for this address: the
    /// row is unspent only as far as has been read. An [`indexing::OutputPage`]
    /// carries no claim a caller could weigh, and its consumer spends what it is
    /// given, so the honest answer is a refusal — the retryable `Conflict` every
    /// caller of the port already restarts on.
    ///
    /// The store makes it, from the snapshot the rows come from. Nothing that
    /// reads the page afterwards can, without reading the observations at
    /// another instant than the rows.
    pub async fn offers_no_output_until_the_range_is_read(&self) {
        self.prepare().await;
        self.filled(BlockPosition(1), BlockPosition(3)).await;

        let error = self
            .fixture
            .repository
            .list(self.fixture.request("late", 50))
            .await
            .expect_err("rows nothing can prove unspent must not be offered");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert!(
            error.retryable,
            "the refusal clears as the catch-up reaches the line"
        );

        self.filled(BlockPosition(4), BlockPosition(5)).await;
        assert!(
            self.outputs().await.is_empty(),
            "read through the line, the set is offered — and block 5 spent the \
             output block 3 created"
        );
    }

    /// An address registered after its history began ends up with that
    /// history: the rows appear below a checkpoint that does not move, and the
    /// observation's own claim reaches the line.
    pub async fn fills_history_below_the_checkpoint(&self) {
        self.prepare().await;
        assert!(self.history("late").await.is_empty(), "the gap this closes");

        self.filled(BlockPosition(1), BlockPosition(5)).await;

        assert_eq!(
            self.history("late").await,
            ["block-1", "block-2", "block-3", "block-4", "block-5"],
            "every block below the checkpoint is now indexed for the address"
        );
        assert_eq!(
            self.history("payer").await.len(),
            5,
            "and for the counterparty it paid, exactly as the forward half \
             writes them"
        );
        assert_eq!(
            self.checkpoint().await,
            self.fixture.block(5),
            "a batch of blocks the network already committed moves nothing"
        );
        assert_eq!(
            self.progress().await,
            Some(self.fixture.block(5)),
            "the batch records the claim it proved, in the same write"
        );
    }

    /// A batch interrupted halfway resumes where it stopped and reaches the
    /// same end state as one that ran straight through.
    pub async fn resumes_where_an_interrupted_batch_stopped(&self) {
        self.prepare().await;
        self.filled(BlockPosition(1), BlockPosition(3)).await;

        assert_eq!(
            self.progress().await,
            Some(self.fixture.block(3)),
            "half of the range is still owed, and the claim says exactly which"
        );

        self.filled(BlockPosition(4), BlockPosition(5)).await;

        assert_eq!(
            self.history("late").await,
            ["block-1", "block-2", "block-3", "block-4", "block-5"],
            "two batches reach what one batch reaches"
        );
        assert_eq!(
            self.progress().await,
            Some(self.fixture.block(5)),
            "and a resumed read settles exactly as an uninterrupted one does"
        );
    }

    /// Redoing a batch whose outcome was lost writes nothing new and fails
    /// nothing: every row is insert-if-absent.
    ///
    /// **Redone, not re-presented.** A claim is a compare-and-set that the first
    /// commit itself makes stale, so the very same batch again is refused
    /// retryably; the redo that lands is the one built against the progress the
    /// store now holds, and the rows are what make that free.
    pub async fn redoes_a_batch_without_duplicating_it(&self) {
        self.prepare().await;
        let (from, to) = (BlockPosition(1), BlockPosition(5));
        let stale = self.batch(from, to, self.checkpoint().await, self.progress().await);
        self.filled(from, to).await;
        let before = self.history("late").await;

        let refused = self.fixture.repository.commit(stale).await;
        assert!(
            refused.is_err_and(|error| error.kind == IndexErrorKind::Conflict && error.retryable),
            "the same batch again is a stale claim, and reloading it is the recovery"
        );
        self.filled(from, to).await;

        assert_eq!(
            self.history("late").await,
            before,
            "a redone batch leaves one row per block, not two"
        );
    }

    /// A batch planned under a checkpoint that has since moved is refused
    /// whole, retryably, and writes nothing.
    ///
    /// The batch itself is valid — its blocks are below the checkpoint it
    /// names — so what the store refuses is the *plan*, not the content: the
    /// checkpoint is the one thing a write below the line has in place of parent
    /// linkage, and a batch planned under a different one may have been
    /// planned against a different watch set.
    pub async fn refuses_a_batch_whose_checkpoint_moved(&self) {
        self.prepare().await;
        let batch = self.batch(
            BlockPosition(1),
            BlockPosition(3),
            self.fixture.block(3),
            None,
        );

        let error = self
            .fixture
            .repository
            .commit(batch)
            .await
            .expect_err("a stale checkpoint is refused");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert!(error.retryable, "the next pass re-plans and redoes it");
        assert!(
            self.history("late").await.is_empty(),
            "a refused batch writes nothing at all"
        );
    }

    /// A claim whose expected progress is not what the store holds is refused
    /// whole and retryably, and nothing it carried is written.
    ///
    /// **This is the guard, and it is deliberately not monotonic.** A batch redone
    /// under a smaller budget can honestly settle *less* than the write it is
    /// redoing, so a store refusing "it went down" would wedge that observation
    /// for good. What is refused is a claim whose author was reading a different
    /// value — which is how an index finds out that a write it lost the outcome
    /// of actually landed, and the one thing that makes it read its list again
    /// rather than retrying the same refusal forever.
    pub async fn refuses_a_claim_that_expected_other_progress(&self) {
        self.prepare().await;
        self.filled(BlockPosition(1), BlockPosition(3)).await;
        let checkpoint = self.checkpoint().await;

        // The store holds block 3. This claim says it holds nothing.
        let stale = self.batch(BlockPosition(4), BlockPosition(5), checkpoint.clone(), None);
        let error = self
            .fixture
            .repository
            .commit(stale)
            .await
            .expect_err("a claim that expected other progress");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert!(error.retryable, "the index reads its own list again");
        assert_eq!(
            self.progress().await,
            Some(self.fixture.block(3)),
            "and the claim it refused wrote nothing"
        );
        assert_eq!(
            self.history("late").await.len(),
            3,
            "nor did the blocks that came with it"
        );

        // The same batch, with the value the store actually holds, is accepted.
        let honest = self.batch(
            BlockPosition(4),
            BlockPosition(5),
            checkpoint,
            Some(self.fixture.block(3)),
        );
        self.fixture
            .repository
            .commit(honest)
            .await
            .expect("a claim that expected what is held");
        assert_eq!(self.progress().await, Some(self.fixture.block(5)));
    }

    /// A claim naming an observation the store does not hold is refused whole.
    ///
    /// Progress is evidence about a registration, so there is nothing for it to
    /// attach to; accepting it would leave a claim no `WatchStore::all` could ever
    /// return and no `forget` could ever remove.
    pub async fn refuses_a_claim_for_an_unknown_observation(&self) {
        self.prepare().await;
        let checkpoint = self.checkpoint().await;
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            Some(checkpoint.clone()),
            Vec::new(),
            vec![Progress {
                watch_id: "never-registered".to_owned(),
                expected: None,
                indexed_through: checkpoint,
            }],
            FeeSettlement::Charged,
        )
        .expect("a valid batch naming an unknown observation");

        let error = self
            .fixture
            .repository
            .commit(batch)
            .await
            .expect_err("an unknown observation");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(self.progress().await, None);
    }

    /// A batch is one write: a row that disagrees anywhere in it rolls back the
    /// rows and the claims of every block in it.
    ///
    /// The refusal is arranged to happen *after* two blocks of new rows are
    /// already inserted and *before* the claim that would cover them, so the only
    /// way the store can come out unchanged is a whole rollback.
    pub async fn refuses_a_batch_whole_when_one_block_disagrees(&self) {
        self.prepare().await;
        self.seed(5, 5).await;
        assert_eq!(
            self.history("late").await,
            ["block-5"],
            "one block's rows are stored and nothing is claimed yet"
        );

        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            Some(self.fixture.block(5)),
            vec![self.canonical(3), self.canonical(4), self.disagreeing(5)],
            vec![Progress {
                watch_id: self.watch().id,
                expected: None,
                indexed_through: self.fixture.block(4),
            }],
            FeeSettlement::Charged,
        )
        .expect("a valid batch whose last block disagrees with the store");

        let error = self
            .fixture
            .repository
            .commit(batch)
            .await
            .expect_err("a disagreeing row");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "retrying cannot repair the store");
        assert_eq!(
            self.history("late").await,
            ["block-5"],
            "the rows of blocks 3 and 4 were written before the refusal and \
             must be gone"
        );
        assert_eq!(
            self.progress().await,
            None,
            "and the claim it did not prove was not recorded"
        );
    }

    /// A stored row that disagrees with the one being written is corruption:
    /// terminal, and never mistaken for a benign duplicate.
    pub async fn refuses_a_row_that_disagrees_with_the_stored_one(&self) {
        self.prepare().await;
        self.filled(BlockPosition(1), BlockPosition(5)).await;

        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            Some(self.fixture.block(5)),
            vec![self.disagreeing(2)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch");

        let error = self
            .fixture
            .repository
            .commit(batch)
            .await
            .expect_err("a disagreeing row is corruption");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "retrying cannot repair the store");
    }

    /// A batch carrying the block that *is* the line writes the facts it carries
    /// beyond the ones the store already holds.
    ///
    /// The block at the checkpoint is a block the network already committed, so
    /// it is filled in like any other: a commit whose outcome was lost, or an
    /// observation that owes that block and was not watched when it was first
    /// committed, presents it again and the rows that were missing are written.
    /// Matching the block reference alone proves nothing about the transactions,
    /// credits and outputs beside it, which is what the one insert-or-verify rule
    /// says instead.
    pub async fn writes_what_a_block_at_the_line_carries_beyond_the_stored_facts(&self) {
        self.prepare().await;
        assert!(
            self.history("late").await.is_empty(),
            "the fixture committed block 5 with nothing watched"
        );

        self.filled(BlockPosition(5), BlockPosition(5)).await;

        assert_eq!(
            self.history("late").await,
            ["block-5"],
            "the facts the batch carried are held, not dropped"
        );
        assert_eq!(
            self.checkpoint().await,
            self.fixture.block(5),
            "and the line did not move, because the block was already on it"
        );
    }

    /// A block at the line whose facts disagree with the stored ones is
    /// corruption: terminal, never a benign duplicate.
    ///
    /// One `(address, height, transaction)` cannot honestly have two contents,
    /// which is the one thing a re-presentation can never carry and the reason
    /// both directions share one rule instead of the forward one trusting a block
    /// reference.
    pub async fn refuses_a_block_at_the_line_whose_facts_disagree(&self) {
        self.prepare().await;
        self.filled(BlockPosition(5), BlockPosition(5)).await;

        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            Some(self.fixture.block(5)),
            vec![self.disagreeing(5)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch");

        let error = self
            .fixture
            .repository
            .commit(batch)
            .await
            .expect_err("one row cannot honestly have two contents");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "retrying cannot repair the store");
    }

    /// A credit the block made with no transaction is filled in below the
    /// checkpoint like any other change, and redoing the batch does not
    /// count it twice.
    ///
    /// Without it a withdrawal-funded address watched from its own past would
    /// come back permanently short, and nothing would say so: the history
    /// rows would all be there (`docs/design/021` section 4.1).
    pub async fn fills_in_a_credit_no_transaction_names(&self) {
        self.prepare().await;
        assert_eq!(self.settled().await, Decimal::zero(), "the gap this closes");

        self.filled(BlockPosition(1), BlockPosition(5)).await;

        assert_eq!(
            self.settled().await,
            Decimal::from(57_u64),
            "five blocks paying 10 each, plus the 7 block 4 credited"
        );

        self.filled(BlockPosition(1), BlockPosition(5)).await;

        assert_eq!(
            self.settled().await,
            Decimal::from(57_u64),
            "a redone batch verifies every row and credit it already wrote, so \
             nothing is counted twice"
        );
    }

    /// Progress is per network, and a read of one never sees another's.
    pub async fn records_progress_for_its_own_network_only(&self) {
        self.prepare().await;
        self.filled(BlockPosition(1), BlockPosition(5)).await;

        let other = Network::new(
            self.fixture.network.chain.clone(),
            format!("{}-other", self.fixture.network.name),
        );
        let elsewhere = self
            .fixture
            .repository
            .all(&other)
            .await
            .expect("observations");
        assert!(
            elsewhere.is_empty(),
            "one store holds every network, and progress recorded for one is \
             invisible to a read of another: the isolation is the key, not a \
             repository bound to a network",
        );
        assert_eq!(
            self.progress().await,
            Some(self.fixture.block(5)),
            "this one has its"
        );
    }

    /// Forgetting an observation removes its claim and nothing else, and
    /// forgetting what was never recorded is not an error.
    ///
    /// The durable half of an `unwatch`: the rows are facts about blocks, the
    /// claim is evidence about an observation, and only the second stops being
    /// true when the observation goes away.
    pub async fn forgets_one_observations_progress_and_keeps_its_rows(&self) {
        self.prepare().await;
        self.filled(BlockPosition(1), BlockPosition(5)).await;
        let before = self.history("late").await;
        assert!(!before.is_empty(), "the batch wrote the address's history");

        self.fixture
            .repository
            .forget(&self.fixture.network, &self.watch())
            .await
            .expect("forgetting recorded progress");
        let unknown = Watch {
            id: "never-watched".to_owned(),
            address: self.fixture.address("never-watched"),
            ..self.watch()
        };
        self.fixture
            .repository
            .forget(&self.fixture.network, &unknown)
            .await
            .expect("forgetting what was never recorded is not an error");

        assert_eq!(
            self.progress().await,
            None,
            "the claim is gone, so nothing inherits it"
        );
        assert_eq!(
            self.history("late").await,
            before,
            "every row the batch wrote is still there"
        );
    }
}
