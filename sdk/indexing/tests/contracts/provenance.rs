//! What a store may claim about the range behind the rows it answers with, and
//! that it claims the same thing to both reads.
//!
//! Split from `history.rs` by role, for the reason `anchors.rs` and `chunk.rs`
//! state: one file per guarantee under the repository's 500-line cap.
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
//! **One axis the two do not share**, and the last scenario is it.
//! `Completeness::Overdrawn` is what the *arithmetic* proved about one holding —
//! more left the address than ever reached it — and a page performs no
//! arithmetic to prove it with. So the range claim is the whole of what a page
//! answers, and a store that let the fold's proof reach one would be reporting a
//! statement about a total on a list of transactions.

use indexing::{
    BalanceQuery, BalanceStore, BlockPosition, Completeness, ConfirmedHistory, IndexStore,
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
        self.fixture
            .repository
            .add(vec![Watch {
                id: id.to_owned(),
                address: self.fixture.address(address),
                asset: self.fixture.native(),
                start,
            }])
            .await
            .expect("the observation");
    }

    /// Records that `id` has been inspected through the line the fixture left
    /// behind, which is the only way a claim is written at all.
    ///
    /// The batch carries no block: the ten it would carry are already stored,
    /// and the line it leaves behind is the block the claim names.
    pub(super) async fn inspected(&self, id: &str) {
        let line = self.fixture.block(10);
        self.fixture
            .repository
            .commit(
                IndexedBatch::new(
                    self.fixture.network.clone(),
                    Some(line.clone()),
                    Vec::new(),
                    vec![Progress {
                        watch: id.to_owned(),
                        expected: None,
                        indexed_through: line,
                    }],
                    self.fixture.settlement,
                )
                .expect("a claim on the line the fixture left behind"),
            )
            .await
            .expect("the claim");
    }
}
