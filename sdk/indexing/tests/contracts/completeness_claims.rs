//! What a store may claim about the range behind the rows it answers with, and
//! that it claims the same thing to both reads.
//!
//! Split from `history.rs` by role, for the reason `reanchor.rs` and
//! `backfill_fixture.rs` state: one file per guarantee under the repository's
//! 500-line cap.
//!
//! **A page's claim is about the stream, not the window.** The rows on a page
//! are the rows on that page, so saying they are all there is unfalsifiable. The
//! claim worth making, and the one a caller exporting an address's history
//! needs, is whether walking every page enumerates every transaction the address
//! ever took part in — which is a property of the *observation*, not of the
//! query: it holds exactly when observation began at the chain's first block and
//! something has actually been inspected for it.
//!
//! **Which is why both reads are asserted together.** A balance and a history
//! page project one address's rows two ways, over one observation row, so what
//! is provable about the range behind them is one fact. A store answering it
//! twice could disagree with itself, and a caller holding a complete-looking
//! page beside an incomplete-looking total would have nothing to tell it which
//! of the two had lied. Asserting the pair is what makes that unrepresentable.
//!
//! **Read at the line the answer carries.** How far an address has been
//! inspected is compared with the checkpoint of the same snapshot as the rows,
//! and for every observation of the address rather than the one the query
//! names: one inspection of an address serves every asset observed at it, so a
//! sibling still reading its own past is still inserting the rows this read
//! folds. An index used to weaken a store's claim by exactly these two facts on
//! its way to a caller, from its own memory; it no longer sees the answer, so
//! the store owes them, and three scenarios below hold it to them.
//!
//! **One axis the two do not share**, and
//! `an_overdrawn_fold_does_not_reach_the_page_of_the_same_rows` is it.
//! `Completeness::Overdrawn` is what the *arithmetic* proved about one holding —
//! more left the address than ever reached it — and a page performs no
//! arithmetic to prove it with. So the range claim is the whole of what a page
//! answers, and a store that let the fold's proof reach one would be reporting a
//! statement about a total on a list of transactions.

use indexing::{
    AssetId, BalanceQuery, BalanceStore, BlockPosition, Completeness, ConfirmedHistory, IndexStore,
    IndexedBatch, Progress, Watch, WatchStore,
};

use super::history::HistoryContract;

impl<R: ConfirmedHistory + BalanceStore + IndexStore + WatchStore> HistoryContract<'_, R> {
    /// A whole-history claim needs both halves, and a page makes it under
    /// exactly the conditions the fold of the same rows does.
    ///
    /// Three reads of one address, and only the store's knowledge of the
    /// observation changes between them: unobserved, observed with nothing
    /// inspected, and observed with the range read. A store deriving the page's
    /// claim from anything the *query* carries — the rows returned, `next` being
    /// absent, the page being the first — would answer the same thing at all
    /// three.
    ///
    /// The subject is `b`, whose native holding is credited four times and
    /// debited never, so nothing the fold proves can stand in for the claim
    /// under test.
    pub async fn a_page_claims_a_whole_history_only_once_it_has_been_read(&self) {
        self.prepare().await;
        assert_eq!(
            self.provenance("b").await,
            (Completeness::Incomplete, Completeness::Incomplete),
            "nothing observes this address, so nothing can be claimed about \
             the range behind its rows",
        );

        self.observes("whole", "b", BlockPosition(0)).await;
        assert_eq!(
            self.provenance("b").await,
            (Completeness::Incomplete, Completeness::Incomplete),
            "a birthday says where inspection would start, not that any of it \
             happened",
        );

        self.inspected("whole").await;
        assert_eq!(
            self.provenance("b").await,
            (Completeness::CompleteHistory, Completeness::CompleteHistory),
            "observation began at the first block and every block up to the \
             line has been inspected for it, so both reads may say so",
        );
    }

    /// And the claim is refused for an observation born above the chain's first
    /// block however much of its own range it has read, because the blocks below
    /// a birthday are ones no backfill will ever visit.
    ///
    /// Without this the scenario above would hold for a store that answered
    /// from the recorded progress alone.
    pub async fn a_page_born_above_the_first_block_claims_no_whole_history(&self) {
        self.prepare().await;
        self.observes("late", "b", BlockPosition(3)).await;
        self.inspected("late").await;

        assert_eq!(
            self.provenance("b").await,
            (Completeness::Incomplete, Completeness::Incomplete),
            "blocks 0 to 2 are a range this observation will never read, so \
             neither read may claim to hold the address's whole history",
        );
    }

    /// The one answer the two reads do not share: a fold that proves itself
    /// short reports that proof, and the page of the same rows keeps answering
    /// the range question it was asked.
    ///
    /// `a` pays a fee on seven transactions and is never credited in the native
    /// asset, so its fold is provably missing credits. That proof is about a
    /// *total*; the page lists the transactions `a` took part in, every one of
    /// which is there, and it has no arithmetic to contradict itself with. A
    /// store reusing the balance's answer wholesale would put `Overdrawn` on a
    /// page, which says nothing a caller of that page can act on.
    pub async fn an_overdrawn_fold_does_not_reach_the_page_of_the_same_rows(&self) {
        self.prepare().await;
        self.observes("short", "a", BlockPosition(0)).await;
        self.inspected("short").await;

        assert_eq!(
            self.provenance("a").await,
            (Completeness::CompleteHistory, Completeness::Overdrawn),
            "the rows prove the total is short and prove nothing about which \
             transactions are listed",
        );
    }

    /// A whole history is not claimed while the range behind it is still being
    /// read, however early the observation began.
    ///
    /// The observation starts at the chain's first block and progress *has*
    /// been recorded for it — through block 5, under a line at 10. Both halves
    /// of the older rule hold, and the claim is still unearned: blocks 6 to 10
    /// are stored for the network and have not been inspected for this address,
    /// so a credit in any of them is a row this read does not have. A store
    /// comparing the progress with the birthday, rather than with the line its
    /// answer is stamped with, claims `CompleteHistory` here.
    pub async fn a_whole_history_is_not_claimed_below_the_line_it_is_read_at(&self) {
        self.prepare().await;
        self.observes("behind", "b", BlockPosition(0)).await;
        self.inspected_through("behind", None, 5).await;

        assert_eq!(
            self.provenance("b").await,
            (Completeness::Incomplete, Completeness::Incomplete),
            "inspected through 5 and read at 10: five blocks nothing has read \
             for this address",
        );

        self.inspected_through("behind", Some(5), 10).await;
        assert_eq!(
            self.provenance("b").await,
            (Completeness::CompleteHistory, Completeness::CompleteHistory),
            "and the claim is earned the moment the inspection reaches the line",
        );
    }

    /// Another observation of the same address that is still reading its own
    /// past weakens every read of the address, whatever asset each is in.
    ///
    /// The native observation is caught up, and until the token observation of
    /// the same address is registered its reads claim a whole history. The token
    /// one owes the range below the line, and the rows it is still inserting are
    /// rows of the address the native read folds. Once it reaches the line the
    /// claim is earned again, which is what stops the middle assertion passing
    /// for a store that never claims anything.
    pub async fn a_sibling_still_catching_up_weakens_every_read_of_the_address(&self) {
        self.prepare().await;
        self.observes("native", "b", BlockPosition(0)).await;
        self.inspected("native").await;
        assert_eq!(
            self.provenance("b").await,
            (Completeness::CompleteHistory, Completeness::CompleteHistory),
            "nothing at this address is owed yet",
        );

        self.observes_in("token", "b", self.fixture.token(), BlockPosition(3))
            .await;
        assert_eq!(
            self.provenance("b").await,
            (Completeness::Incomplete, Completeness::Incomplete),
            "the token observation of the same address owes blocks 3 to 10",
        );

        self.inspected("token").await;
        assert_eq!(
            self.provenance("b").await,
            (Completeness::CompleteHistory, Completeness::CompleteHistory),
            "and nothing weakens the claim once the address owes nothing",
        );
    }

    /// What weakens a read is what *its own address* owes, and only what is
    /// owed below the line.
    ///
    /// Two observations that must change nothing: one of another address,
    /// behind by the whole chain, and one of this address born above the line,
    /// which has no progress and owes none. A store that weakened on any
    /// observation of the network, or on any observation without recorded
    /// progress, would answer `Incomplete` here and would pass both scenarios
    /// above.
    pub async fn only_what_the_address_owes_below_the_line_weakens_its_reads(&self) {
        self.prepare().await;
        self.observes("native", "b", BlockPosition(0)).await;
        self.inspected("native").await;
        self.observes("elsewhere", "c", BlockPosition(0)).await;
        self.observes_in("fresh", "b", self.fixture.token(), BlockPosition(11))
            .await;

        assert_eq!(
            self.provenance("b").await,
            (Completeness::CompleteHistory, Completeness::CompleteHistory),
            "another address's backlog, and a sibling born above the line, are \
             not rows this address is waiting for",
        );
    }

    /// A whole history is not claimed over a network that has committed
    /// nothing, however early the observation began.
    ///
    /// The one state in which an observation is behind no line and has still
    /// read nothing: there is no line to be behind
    /// (`indexing::WatchProgress::caught_up`), its birthday is the chain's first
    /// block, and no progress has been recorded for it. A store that took
    /// "owes nothing" for "has been read" claims `CompleteHistory` here, over
    /// no block at all.
    ///
    /// No fixture is written first, which is the scenario: the repository
    /// holds the observation and nothing else.
    pub async fn a_whole_history_is_not_claimed_before_the_first_commit(&self) {
        self.observes("early", "b", BlockPosition(0)).await;

        assert_eq!(
            self.provenance("b").await,
            (Completeness::Incomplete, Completeness::Incomplete),
            "born at the first block and behind nothing, and nothing has been \
             inspected for it",
        );
    }

    /// What the store claims about one address's range, to each read in turn:
    /// the page first, then the fold of the very same rows.
    async fn provenance(&self, address: &str) -> (Completeness, Completeness) {
        let page = self
            .fixture
            .repository
            .history(self.query(address, &self.fixture.native(), None, 10))
            .await
            .expect("a history page");
        let balance = self
            .fixture
            .repository
            .balance(BalanceQuery {
                address: self.fixture.address(address),
                asset: self.fixture.native(),
            })
            .await
            .expect("the settled balance");
        (page.completeness, balance.completeness)
    }

    /// Registers `id` watching `address`'s native holding from `start`.
    pub(super) async fn observes(&self, id: &str, address: &str, start: BlockPosition) {
        self.observes_in(id, address, self.fixture.native(), start)
            .await;
    }

    /// Registers `id` watching `address`'s holding of `asset` from `start`.
    async fn observes_in(&self, id: &str, address: &str, asset: AssetId, start: BlockPosition) {
        self.fixture
            .repository
            .add(vec![Watch {
                id: id.to_owned(),
                address: self.fixture.address(address),
                asset,
                start,
            }])
            .await
            .expect("the observation");
    }

    /// Records that `id` has been inspected through the line the fixture left
    /// behind, which is the only way a claim is written at all.
    pub(super) async fn inspected(&self, id: &str) {
        self.inspected_through(id, None, 10).await;
    }

    /// Records that `id` has been inspected through block `height`, at or below
    /// the line the fixture left behind, from the progress it `expected`.
    ///
    /// The batch carries the block the claim names and nothing else. It is
    /// already stored, so the store verifies it rather than writing it, and the
    /// line stays where it was: this is one batch of a catch-up.
    async fn inspected_through(&self, id: &str, expected: Option<u64>, height: u64) {
        let line = self.fixture.block(10);
        let named = self
            .fixture
            .chain(self.drafts())
            .into_iter()
            .find(|canonical| canonical.block().position == BlockPosition(height))
            .expect("a block of the fixture chain");
        let claim = Progress {
            watch_id: id.to_owned(),
            expected: expected.map(|height| self.fixture.block(height)),
            indexed_through: named.block().clone(),
        };
        self.fixture
            .repository
            .commit(
                IndexedBatch::new(
                    self.fixture.network.clone(),
                    Some(line),
                    vec![named],
                    vec![claim],
                    self.fixture.settlement,
                )
                .expect("a claim at or below the line the fixture left behind"),
            )
            .await
            .expect("the claim");
    }
}
