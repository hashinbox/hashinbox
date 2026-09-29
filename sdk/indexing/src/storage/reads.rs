//! What a store may offer beyond [`crate::Repository`], in one file because one
//! rule governs all three: a store that offers a capability owes its whole
//! contract, including the consistent read [`crate::IndexStore`] states.
//!
//! The read vocabulary each one names lives beside the trait that names it, not
//! here — [`OutputRequest`] beside [`crate::IndexedOutput`]. [`BalanceQuery`]
//! and [`PurgedRows`] stay because the port that takes the one and the port that
//! answers the other are the only things that name them.
//!
//! **History is not one of these**, and the difference is the point. A balance
//! is one aggregate and a spendable set is a live projection, so a store port
//! answers each; a *page* is a reader's own request, with a length and a
//! resume position, and no sink should carry that model to be a sink. Querying
//! history is the repository's own API ([`crate::ConfirmedHistory`]), declared
//! with the query it takes rather than here among the ports.

use crate::{
    Address, AssetId, Balance, BoxFuture, Holder, IndexError, Network, OutputPage, OutputRequest,
};

/// What a store folds one balance over: whose holding it is, and nothing else.
///
/// It used to carry the chain rule that says whether the network fee is already
/// inside the movements, because the fold had to decide per read whether to
/// charge it. That rule is consulted once now, at commit
/// ([`IndexedBatch::settlement`](crate::IndexedBatch::settlement)), and its
/// answer is among the rows — so a reader that was told the wrong rule is a
/// state this query can no longer be in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BalanceQuery {
    /// Whose balance; its network is the query network.
    pub address: Address,
    /// Which asset it is read in; must be of `address`'s chain.
    pub asset: AssetId,
}

impl BalanceQuery {
    /// The query that folds `of`'s holding.
    #[must_use]
    pub fn new(of: &dyn Holder) -> Self {
        Self {
            address: of.address().clone(),
            asset: of.asset().clone(),
        }
    }
}

impl Holder for BalanceQuery {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &AssetId {
        &self.asset
    }
}

/// The settled half of a balance, folded where the rows are.
///
/// Deliberately a store port rather than a read-side fold over history pages:
/// a page is bounded and ordered for a reader, while a balance is one
/// aggregate the store can answer in a single ordered scan, at one
/// checkpoint, without the caller paging an address's whole history to add it
/// up (`docs/design/021` section 5.3).
///
/// The returned [`Balance`] carries the store's own checkpoint as
/// `observed_at` and nothing pending; the blocks above the confirmed line are
/// the index's half, folded in by [`Balances`](crate::Balances).
pub trait BalanceStore: Send + Sync {
    /// Folds every stored movement of `query`'s holding into its balance.
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>>;
}

/// The forwarding impl every port here carries, so a store that cannot be cloned
/// is used as `Arc<MySink>`.
impl<T: BalanceStore + ?Sized> BalanceStore for std::sync::Arc<T> {
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>> {
        (**self).balance(query)
    }
}

/// Current live outputs indexed for canonical addresses.
pub trait OutputStore: Send + Sync {
    /// Lists one checkpoint-consistent page of spendable outputs.
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>>;
}

/// The same forwarding impl, for the same reason.
impl<T: OutputStore + ?Sized> OutputStore for std::sync::Arc<T> {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        (**self).list(request)
    }
}

/// Taking one network's index back to nothing, offline.
///
/// Deliberately **not** part of [`crate::Repository`]: it is the offline
/// rescan, not indexing, and a third-party sink that does not offer one still
/// indexes correctly. The built-in store and the server's rescan tool are its
/// only consumers.
pub trait Purging: Send + Sync {
    /// Atomically removes every indexing-owned row of exactly one network —
    /// its checkpoint, history, movements, block credits, and live outputs —
    /// and reports how many of each it removed.
    ///
    /// The offline rescan (`docs/design/019` section 5.3), and the only way
    /// back from an [`crate::IndexErrorKind::ConfirmedReorg`].
    ///
    /// **The observations survive and their progress does not.** What each
    /// watch observes is application state and is never touched, so the next
    /// start finds the same list; what was inspected for it is a claim about
    /// rows this purge just removed, so every
    /// [`crate::WatchProgress::indexed_through`] of this network is reset to
    /// `None` in the same transaction. Leaving one behind would let a watch
    /// claim a range nothing has read since, and neither [`crate::WatchStore`]
    /// method can undo that afterwards — `add` preserves progress on an exact
    /// repeat, by contract.
    ///
    /// Only this network's rows: every other network in the same store is
    /// untouched, which is what makes one shared store safe to purge one
    /// network of.
    ///
    /// Callers must run it offline. Purging while HTTP serves history from
    /// this network would hand clients an empty history that looks
    /// authoritative.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::Store`] when the removal itself fails.
    fn purge_network<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<PurgedRows, IndexError>>;
}

/// What one [`Purging::purge_network`] removed, per stored collection.
///
/// The collections `docs/design/019` section 5.3 names, and the block credits
/// `docs/design/021` section 4.4 added. A store that keeps value movements in a
/// collection of their own reports them here; one that nests a transaction's
/// movements inside its history entry removes them with `history` and reports
/// no movement rows of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgedRows {
    /// The checkpoint: one when the network had committed a block, zero when
    /// it had not.
    pub checkpoint: u64,
    /// Canonical history entries: one per watched address per transaction.
    pub history: u64,
    /// Value-movement rows, where the backend stores them separately.
    pub movements: u64,
    /// Live (unspent) output rows.
    pub outputs: u64,
    /// Block-credit rows: one per credited address per transactionless credit.
    /// Zero on a chain that has none, which is every chain but Ethereum.
    pub credits: u64,
}

impl<T: Purging + ?Sized> Purging for std::sync::Arc<T> {
    fn purge_network<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        (**self).purge_network(network)
    }
}
