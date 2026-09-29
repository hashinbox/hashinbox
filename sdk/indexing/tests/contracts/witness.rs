//! What an index may pass on to a page, over a store that claims more than a
//! page can carry.
//!
//! Split from `provenance.rs` by role, for the reason `anchors.rs` and
//! `chunk.rs` state: one file per guarantee. The guarantee here is not the
//! store's — `provenance.rs` owns what a store may claim — but the index's, and
//! it needs a running [`Service`] over the store rather than the store alone.
//!
//! **One vocabulary on the wire, narrowed at the boundary.**
//! `Completeness::VerifiedOpening` is what an independent node read agreeing
//! with the fold at one block proves (`chain_ethereum::BalanceWitness`), and it
//! is evidence about a *total*. A balance reports a total and may carry it; a
//! history page reports which transactions an address took part in and reports
//! no total at all, so on a page the value says something about a number the
//! page does not contain. Both reads keep the one enum `apps/api` serves — the
//! page path narrows instead, and this is that narrowing held to both stores.
//!
//! **Neither shipped store produces the value**, which is exactly why the
//! scenario cannot be written over one: a fixture that cannot express the
//! failure is `docs/DEFECT_PATTERNS.md` section 4.2. So the store is wrapped in
//! one that claims it on every read, the way `backfill_contract.rs` wraps one
//! to make its gates falsifiable.

use indexing::{
    Address, Balance, BalanceQuery, BalanceStore, Balances, BlockInterpreter, BlockPosition,
    BlockRef, BoxFuture, CanonicalPage, Completeness, ConfirmedHistory, FeeSettlement, History,
    HistoryQuery, IndexError, IndexStore, IndexedBatch, IndexedBlock, InterpretedBlock, Network,
    OutputKey, Page, Service, Start, Subject, SyncConfig, SyncPolicy, Watch, WatchProgress,
    WatchStore, Watching,
};

use super::history::HistoryContract;

/// The chain the index never reaches: every block this scenario needs is
/// already committed through the store, so nothing fetches or interprets one.
struct Chain;

/// A native block that is only ever its own reference.
#[derive(Clone)]
struct Native(BlockRef);

impl IndexedBlock for Native {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

impl BlockInterpreter for Chain {
    type Block = Native;

    fn inspect(
        &self,
        _block: &Self::Block,
        _addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        unreachable!("this scenario commits through the store and only reads")
    }

    fn settlement(&self) -> FeeSettlement {
        FeeSettlement::Charged
    }
}

/// The store under test with one answer changed: both of its reads claim a
/// verified opening, whatever the rows say.
///
/// A borrow rather than a clone, because the contract holds one and the
/// scenario must read the very rows the fixture wrote.
struct Attested<'a, R>(&'a R);

impl<R> Clone for Attested<'_, R> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<R: ConfirmedHistory> ConfirmedHistory for Attested<'_, R> {
    fn history<'a>(
        &'a self,
        query: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move {
            let mut page = self.0.history(query).await?;
            page.completeness = Completeness::VerifiedOpening;
            Ok(page)
        })
    }
}

impl<R: BalanceStore> BalanceStore for Attested<'_, R> {
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            let mut balance = self.0.balance(query).await?;
            balance.completeness = Completeness::VerifiedOpening;
            Ok(balance)
        })
    }
}

impl<R: IndexStore> IndexStore for Attested<'_, R> {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.0.checkpoint(network)
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        self.0.commit(batch)
    }
}

impl<R: WatchStore> WatchStore for Attested<'_, R> {
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        self.0.add(watches)
    }

    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        self.0.all(network)
    }

    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        self.0.forget(network, watch)
    }

    fn reanchor<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        self.0.reanchor(watches)
    }
}

impl<R: ConfirmedHistory + BalanceStore + IndexStore + WatchStore> HistoryContract<'_, R> {
    /// Evidence that a node read agreed with the fold about a total reaches the
    /// read that reports that total, and never the page of the same rows.
    ///
    /// The observation begins at the chain's first block and has been inspected
    /// through the line, which is the state under which nothing else weakens a
    /// claim at all — `a_page_claims_a_whole_history_only_once_it_has_been_read`
    /// is the same fixture reaching `CompleteHistory` on both reads.
    ///
    /// **Which is what makes the other two narrowings provably no-ops here, and
    /// the balance assertion is the proof.** Both of them run on the same
    /// address, over the same watch set, in the same moment as the page read,
    /// and the fold comes back carrying the store's claim untouched. So neither
    /// can be what narrowed the page. Reverse it — let the address owe a
    /// backfill — and `while_backfilling` maps this very value to `Incomplete`
    /// on its own, and the scenario would pass with the page narrowing deleted.
    ///
    /// The subject is `b`, credited four times in the native asset and debited
    /// never: `a` is overdrawn on this fixture, and an overdrawn fold is a
    /// second thing the store would answer differently.
    pub async fn a_witnessed_total_never_reaches_the_page_of_the_same_rows(&self) {
        self.prepare().await;
        self.observes("witnessed", "b", BlockPosition(0)).await;
        self.inspected("witnessed").await;

        let index = Service::new(
            (),
            Chain,
            Attested(self.fixture.repository),
            SyncConfig::new(&self.fixture.network, SyncPolicy::new(10)).expect("a valid policy"),
        );
        let of = Subject {
            address: self.fixture.address("b"),
            asset: self.fixture.native(),
        };
        index
            .watch("witnessed", &of, Start::At(BlockPosition(0)))
            .await
            .expect("the observation the store already holds");

        let page = index
            .history()
            .history(&of, Page::first(10))
            .await
            .expect("a history page");
        let balance = index
            .balances()
            .balance(&of)
            .await
            .expect("the settled balance");

        assert_eq!(
            balance.completeness,
            Completeness::VerifiedOpening,
            "the fold reports the total the node agreed about, and nothing \
             this address owes weakens it — which is what leaves the page \
             narrowing as the only thing that can act below",
        );
        assert_eq!(
            page.completeness,
            Completeness::Incomplete,
            "a page reports no total, so evidence about one cannot ride on it",
        );
    }
}
