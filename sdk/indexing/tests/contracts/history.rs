//! The history query contract.

use base::Decimal;

use indexing::{
    AssetId, ConfirmedHistory, ConfirmedHistoryPage, HistoryPosition, HistoryQuery, IndexStore,
    MovementId, Network, NetworkFee, Status, Transaction, TransactionRef, ValueMovement,
};

use super::fixture::Fixture;

/// The [`HistoryQuery`] contract, checked against one repository.
///
/// The fixture writes one transaction per block, heights 1 to 10, for the
/// addresses `a`, `b`, and `c`, the native asset `N`, and a token `T`:
///
/// | Height | Id | Movements | Fee payer |
/// |---|---|---|---|
/// | 1-4 | `n1`..`n4` | `N` a→b | a |
/// | 5 | `t5` | `T` c→a | c |
/// | 6 | `m6` | `T` a→b, `N` a→c | a |
/// | 7 | `e7` | `T` b→c, `N` c→a | c |
/// | 8 | `f8` | none (failed) | a |
/// | 9 | `p9` | `T` b→c | a |
/// | 10 | `t10` | `T` a→c | a |
pub struct HistoryContract<'a, R> {
    pub(super) fixture: Fixture<'a, R>,
}

impl<'a, R: IndexStore + ConfirmedHistory> HistoryContract<'a, R> {
    /// Checks `repository`, which must hold nothing for `network` yet.
    #[must_use]
    pub const fn new(repository: &'a R, network: Network) -> Self {
        Self {
            fixture: Fixture::new(repository, network),
        }
    }

    /// `limit` counts matching transactions only: pages are full, and no
    /// empty tail page follows the last match.
    pub async fn limit_counts_only_the_query_asset(&self) {
        self.prepare().await;
        let first = self.page("a", &self.fixture.token(), None, 2).await;
        assert_eq!(Self::ids(&first), ["t5", "m6"]);
        let second = self.page("a", &self.fixture.token(), first.next, 2).await;
        assert_eq!(Self::ids(&second), ["f8", "t10"]);
        assert_eq!(second.next, None, "no transaction of T remains");
        let native = self.page("a", &self.fixture.native(), None, 4).await;
        assert_eq!(Self::ids(&native), ["n1", "n2", "n3", "n4"]);
        assert!(native.next.is_some(), "m6 and later match N");
    }

    /// A mixed transaction carries only its movements of the query asset,
    /// every endpoint, plus its native fee.
    pub async fn returns_only_query_asset_movements(&self) {
        self.prepare().await;
        let token = self.page("a", &self.fixture.token(), None, 2).await;
        let mixed = &token.transactions[1];
        assert_eq!(
            mixed.movements,
            [self.transfer("m6-t", &self.fixture.token(), "a", "b")]
        );
        assert_eq!(mixed.fee, Some(self.fee("a")));
        let native = self.page("b", &self.fixture.native(), None, 10).await;
        assert!(native.transactions.iter().all(|transaction| {
            transaction
                .movements
                .iter()
                .all(|movement| movement.asset == self.fixture.native())
        }));
    }

    /// A transaction stored under an address matches an asset only if the
    /// address is an endpoint of one of that asset's movements (or paid).
    pub async fn requires_the_address_as_endpoint(&self) {
        self.prepare().await;
        let token = self.page("a", &self.fixture.token(), None, 10).await;
        assert!(!Self::ids(&token).contains(&"e7".to_owned()));
        let native = self.page("a", &self.fixture.native(), None, 10).await;
        let routed = native
            .transactions
            .iter()
            .find(|transaction| transaction.id.id.as_str() == "e7");
        let movements = routed.map(|transaction| transaction.movements.clone());
        assert_eq!(
            movements,
            Some(vec![self.transfer(
                "e7-n",
                &self.fixture.native(),
                "c",
                "a"
            )])
        );
    }

    /// A failed transaction appears, without movements, in every asset's
    /// history of its fee payer and nowhere else.
    pub async fn failed_fee_payment_appears_for_every_asset(&self) {
        self.prepare().await;
        for asset in [self.fixture.native(), self.fixture.token()] {
            let page = self.page("a", &asset, None, 10).await;
            let failed = page
                .transactions
                .iter()
                .find(|transaction| transaction.id.id.as_str() == "f8");
            assert_eq!(
                failed.map(|transaction| transaction.movements.len()),
                Some(0)
            );
            let other = self.page("b", &asset, None, 10).await;
            assert!(!Self::ids(&other).contains(&"f8".to_owned()));
        }
    }

    /// A successful transaction whose fee the address paid appears in the fee
    /// asset's history even when it moved nothing of it for the address.
    pub async fn successful_fee_payment_appears_for_the_fee_asset_only(&self) {
        self.prepare().await;
        let native = self.page("a", &self.fixture.native(), None, 10).await;
        let paid = native
            .transactions
            .iter()
            .find(|transaction| transaction.id.id.as_str() == "p9");
        assert_eq!(paid.map(|transaction| transaction.movements.len()), Some(0));
        let token = self.page("a", &self.fixture.token(), None, 10).await;
        assert!(!Self::ids(&token).contains(&"p9".to_owned()));
    }

    /// A cursor is a position in the address's history: a native cursor
    /// resumes a token query right after it, with no gap and no duplicate.
    pub async fn cursor_resumes_across_assets(&self) {
        self.prepare().await;
        let native = self.page("a", &self.fixture.native(), None, 5).await;
        assert_eq!(Self::ids(&native), ["n1", "n2", "n3", "n4", "m6"]);
        let token = self.page("a", &self.fixture.token(), native.next, 10).await;
        assert_eq!(Self::ids(&token), ["f8", "t10"]);
        assert_eq!(token.next, None);
    }

    /// A cursor stays exact while writes land after it: what the caller reads
    /// by resuming equals what a reader starting now sees, including the
    /// transactions committed while the cursor was held.
    ///
    /// The confirmed store is append-only, so a held cursor can only be
    /// overtaken from above. A store whose cursor were an offset, a row count
    /// or a snapshot handle would pass every other scenario here and fail this
    /// one.
    pub async fn a_cursor_survives_writes_that_land_after_it(&self) {
        self.prepare().await;
        let native = self.fixture.native();
        let before = self.walk("a", &native, 1_000).await;
        let first = self.page("a", &native, None, 3).await;
        assert_eq!(Self::ids(&first), ["n1", "n2", "n3"]);
        let held = first.next.clone().expect("more native history follows");

        let appended = self.append(&["x11", "x12", "x13"]).await;

        let mut resumed = first.transactions.clone();
        let mut after = Some(held);
        while let Some(cursor) = after {
            let page = self.page("a", &native, Some(cursor), 2).await;
            resumed.extend(page.transactions);
            after = page.next;
        }
        assert_eq!(
            resumed,
            self.walk("a", &native, 1_000).await,
            "resuming a held cursor must equal one fresh read of the same history"
        );
        assert_eq!(
            resumed.len(),
            before.len() + appended,
            "every transaction committed after the first page must be reachable"
        );
    }

    /// A page runs in the chain's own execution order, not in its identities'.
    ///
    /// The three transactions of one block are named **against** their native
    /// places, so ordering by `(height, transaction_id)` — what every store here
    /// did before `transaction_order` existed — returns them exactly backwards,
    /// and a store that merely kept the order it was handed returns them in the
    /// third order this hands them over in. Only a store that pages by the
    /// ordinal the interpreter supplied answers `z, y, x`.
    pub async fn pages_run_in_native_block_order(&self) {
        self.prepare().await;
        let top = self.drafts().len() as u64;
        self.commits(
            top + 1,
            top,
            vec![
                self.placed("y", 1),
                self.placed("x", 2),
                self.placed("z", 0),
            ],
        )
        .await;

        let page = self.page("a", &self.fixture.native(), None, 1_000).await;
        let block: Vec<&Transaction> = page
            .transactions
            .iter()
            .filter(|transaction| transaction.block().height.0 == top + 1)
            .collect();
        let ids: Vec<String> = block
            .iter()
            .map(|transaction| transaction.id.id.to_string())
            .collect();
        assert_eq!(ids, ["z", "y", "x"], "a page is in native block order");
        let orders: Vec<u64> = block.iter().map(|transaction| transaction.order).collect();
        assert_eq!(orders, [0, 1, 2], "the stored ordinal is the one supplied");
    }

    /// One transaction's ordinal is its **block's** own, so a second, wider
    /// indexing of the same block gives it the very same place, and a cursor
    /// issued before that second write still names the transaction it named.
    ///
    /// This is the write a rank computed by a store cannot survive. A store
    /// numbering the transactions it was *handed* would file `tx-b` at 0 when a
    /// narrow observation brought it alone and at 2 when a wider one brought all
    /// three of its block — moving a row underneath a cursor somebody is holding,
    /// and making its own verify-if-present refuse the second commit outright.
    /// The ordinal is the position in the complete block instead, so both writes
    /// present the same number and the cursor keeps pointing at `tx-b`.
    ///
    /// What the cursor owes is stated exactly: resuming it returns what follows
    /// `tx-b` in block order, with no duplicate of it and nothing between them
    /// skipped. The two transactions filled in *below* `tx-b` are below any
    /// cursor issued past them, under this order or any other; what would be a
    /// defect is `tx-b` itself moving, and that is what this refuses.
    ///
    /// The three identities sort against their native places, so the order the
    /// block reads in is checked here too.
    pub async fn an_ordinal_is_the_blocks_own_under_every_selection(&self) {
        self.prepare().await;
        let native = self.fixture.native();
        let top = self.drafts().len() as u64;
        // The narrow observation's pass: only the transaction the chain executed
        // third touched an address it was watching, so only that one is written.
        self.commits(top + 1, top, vec![self.placed("tx-b", 2)])
            .await;
        // One more block, so the page below has somewhere to continue to.
        self.commits(top + 2, top + 1, vec![self.placed("later", 0)])
            .await;

        let narrow = self.walk("a", &native, 1_000).await;
        let at = Self::place_of("tx-b", &narrow);
        assert_eq!(
            narrow[at].order, 2,
            "the block's own ordinal, not the place in what was reported",
        );
        let page = self.page("a", &native, None, at + 1).await;
        let held = page.next.clone().expect("the twelfth block still follows");
        assert_eq!(held.transaction, self.fixture.transaction("tx-b"));
        assert_eq!(held.transaction_order, 2);

        // A wider observation re-inspects the same block and fills in the two
        // transactions the first pass never saw.
        self.commits(
            top + 1,
            top + 2,
            vec![
                self.placed("tx-c", 0),
                self.placed("tx-a", 1),
                self.placed("tx-b", 2),
            ],
        )
        .await;

        let widened = self.walk("a", &native, 1_000).await;
        let filled: Vec<String> = widened
            .iter()
            .filter(|transaction| transaction.block().height.0 == top + 1)
            .map(|transaction| transaction.id.id.to_string())
            .collect();
        assert_eq!(
            filled,
            ["tx-c", "tx-a", "tx-b"],
            "the block reads in its own order, which its identities disagree with",
        );
        assert_eq!(
            widened[Self::place_of("tx-b", &widened)].order,
            2,
            "a second, wider indexing must not move a transaction's place",
        );

        let resumed = self.resume(&native, held).await;
        let expected = widened
            .iter()
            .skip(Self::place_of("tx-b", &widened) + 1)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            resumed, expected,
            "a held cursor resumes at the transaction it named",
        );
    }

    /// Where `id` sits in a walked history; panics when it is not there.
    fn place_of(id: &str, transactions: &[Transaction]) -> usize {
        transactions
            .iter()
            .position(|transaction| transaction.id.id.as_str() == id)
            .unwrap_or_else(|| panic!("{id} must be in the walked history"))
    }

    /// Every transaction of `a`'s history in `asset` after `held`.
    async fn resume(&self, asset: &AssetId, held: HistoryPosition) -> Vec<Transaction> {
        let mut transactions = Vec::new();
        let mut after = Some(held);
        while let Some(cursor) = after {
            let page = self.page("a", asset, Some(cursor), 2).await;
            transactions.extend(page.transactions);
            after = page.next;
        }
        transactions
    }

    /// One native `a` to `b` transfer at a named place in its block, so a block
    /// can hold several and a page can be wrong about the order of them.
    fn placed(&self, id: &str, order: u64) -> Transaction {
        let movement = self.transfer(&format!("{id}-n"), &self.fixture.native(), "a", "b");
        Transaction {
            order,
            ..self.draft(id, vec![movement], "a")
        }
    }

    /// Commits `drafts` as the one block at `height`, against the line the
    /// fixture left at `checkpoint`.
    async fn commits(&self, height: u64, checkpoint: u64, drafts: Vec<Transaction>) {
        let canonical = self.fixture.canonical_block(height, drafts);
        let line = Some(self.fixture.block(checkpoint));
        self.fixture
            .repository
            .commit(self.fixture.batch(line, canonical))
            .await
            .expect("a block the fixture's line authorises");
    }

    /// Commits one native `a` to `b` transfer per id, above the fixture, and
    /// answers how many of them the query asset's history gains.
    async fn append(&self, ids: &[&str]) -> usize {
        let top = self.drafts().len() as u64;
        for (offset, id) in ids.iter().enumerate() {
            let height = top + offset as u64 + 1;
            self.commits(height, height - 1, vec![self.placed(id, 0)])
                .await;
        }
        ids.len()
    }

    /// Walking every page equals selecting the address's whole history in
    /// memory, for every address, asset, and page size.
    pub async fn pages_equal_the_selected_history(&self) {
        self.prepare().await;
        let unknown = AssetId {
            chain: self.fixture.network.chain.clone(),
            asset: "unknown".into(),
        };
        for address in ["a", "b", "c", "d"] {
            for asset in [self.fixture.native(), self.fixture.token(), unknown.clone()] {
                self.walks_as_selected(address, &asset).await;
            }
        }
    }

    /// Every page size walks `address`'s history in `asset` to the selection.
    async fn walks_as_selected(&self, address: &str, asset: &AssetId) {
        let expected = self.selected(address, asset);
        for limit in [1, 2, 3, 1_000] {
            assert_eq!(self.walk(address, asset, limit).await, expected);
        }
    }

    /// Every page of one query in order; each page but the last is full.
    async fn walk(&self, address: &str, asset: &AssetId, limit: usize) -> Vec<Transaction> {
        let mut transactions = Vec::new();
        let mut after = None;
        loop {
            let page = self.page(address, asset, after, limit).await;
            let full = page.transactions.len() == limit;
            transactions.extend(page.transactions);
            let Some(next) = page.next else {
                return transactions;
            };
            assert!(
                full,
                "a page before the last must hold `limit` transactions"
            );
            after = Some(next);
        }
    }

    /// The fixture history of `address` filtered by [`HistoryQuery::select`].
    fn selected(&self, address: &str, asset: &AssetId) -> Vec<Transaction> {
        let query = self.query(address, asset, None, 1);
        self.fixture
            .chain(self.drafts())
            .iter()
            .flat_map(|canonical| {
                canonical
                    .block
                    .transactions()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .filter(|transaction| transaction.addresses().contains(&query.address))
            .filter_map(|transaction| query.select(transaction))
            .collect()
    }

    /// Commits the fixture blocks in order, each authorised by the one before.
    pub(super) async fn prepare(&self) {
        self.fixture.commit(self.fixture.chain(self.drafts())).await;
    }

    async fn page(
        &self,
        address: &str,
        asset: &AssetId,
        after: Option<HistoryPosition>,
        limit: usize,
    ) -> ConfirmedHistoryPage {
        let query = self.query(address, asset, after, limit);
        self.fixture
            .repository
            .history(query)
            .await
            .expect("history page")
    }

    pub(super) fn query(
        &self,
        address: &str,
        asset: &AssetId,
        after: Option<HistoryPosition>,
        limit: usize,
    ) -> HistoryQuery {
        HistoryQuery {
            address: self.fixture.address(address),
            asset: asset.clone(),
            after,
            limit,
        }
    }

    fn ids(page: &ConfirmedHistoryPage) -> Vec<String> {
        let ids = page.transactions.iter();
        ids.map(|transaction| transaction.id.id.to_string())
            .collect()
    }

    pub(super) fn drafts(&self) -> Vec<Transaction> {
        let (native, token) = (self.fixture.native(), self.fixture.token());
        let mut drafts = (1..=4)
            .map(|index| {
                let id = format!("n{index}");
                let movement = self.transfer(&format!("{id}-n"), &native, "a", "b");
                self.draft(&id, vec![movement], "a")
            })
            .collect::<Vec<_>>();
        drafts.extend([
            self.draft("t5", vec![self.transfer("t5-t", &token, "c", "a")], "c"),
            self.draft(
                "m6",
                vec![
                    self.transfer("m6-t", &token, "a", "b"),
                    self.transfer("m6-n", &native, "a", "c"),
                ],
                "a",
            ),
            self.draft(
                "e7",
                vec![
                    self.transfer("e7-t", &token, "b", "c"),
                    self.transfer("e7-n", &native, "c", "a"),
                ],
                "c",
            ),
            self.failed("f8", "a"),
            self.draft("p9", vec![self.transfer("p9-t", &token, "b", "c")], "a"),
            self.draft("t10", vec![self.transfer("t10-t", &token, "a", "c")], "a"),
        ]);
        drafts
    }

    fn draft(&self, id: &str, movements: Vec<ValueMovement>, payer: &str) -> Transaction {
        Transaction {
            id: TransactionRef::new(self.fixture.network.clone(), id),
            movements,
            fee: Some(self.fee(payer)),
            order: 0,
            // Placed in its block by the fixture.
            status: Status::Included {
                block: self.fixture.block(1),
            },
        }
    }

    fn failed(&self, id: &str, payer: &str) -> Transaction {
        Transaction {
            status: Status::Failed {
                block: self.fixture.block(1),
                reason: Some("reverted".into()),
            },
            ..self.draft(id, Vec::new(), payer)
        }
    }

    fn transfer(&self, id: &str, asset: &AssetId, from: &str, to: &str) -> ValueMovement {
        ValueMovement::transfer(
            MovementId(id.into()),
            asset.clone(),
            Decimal::from(5_u64),
            self.fixture.address(from),
            self.fixture.address(to),
        )
    }

    fn fee(&self, payer: &str) -> NetworkFee {
        NetworkFee {
            asset: self.fixture.native(),
            amount: Decimal::from(7_u64),
            payer: Some(self.fixture.address(payer)),
        }
    }
}
