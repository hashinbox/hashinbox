//! **The optional history query layer**: who may be asked for a page, what a
//! page is, and where the next one resumes.
//!
//! None of it is part of the contract a sink implements. [`crate::Repository`]
//! is registration and committing, and a third-party store that answers no
//! history page never names a position, a page length or a pagination cursor
//! (`examples/custom_store/`). Querying is the *repository's* business, and
//! this file is what a repository that chooses to be queryable answers with.
//!
//! **Two traits, because there are two answers.** [`ConfirmedHistory`] is a
//! repository's own `history(query)` API over the rows it holds, at the
//! checkpoint its answer carries. [`History`] is the pending-inclusive read a
//! consumer asks for, and no repository can answer it: the blocks above the
//! confirmed line are an index's own memory. Only something holding *both*
//! halves can, which is [`crate::ObservedHistory`] — declared beside the balance
//! and output reads that fold the very same overlay, so the three cannot
//! disagree about what is confirmed.

use crate::{
    Address, AssetId, BlockHeight, BlockRef, BoxFuture, CanonicalStatus, CanonicalTransaction,
    Completeness, Decimal, Holder, IndexError, IndexErrorKind, ObservedTransaction, TransactionRef,
    ValueMovement, holder::require_holding,
};

/// The longest page any query asks for, and therefore the longest any store
/// returns.
const MAX_PAGE: usize = 1_000;

/// Rejects a page length no store answers.
///
/// Stated with the requests that carry a limit rather than by each store that
/// would otherwise bound it, because the range is part of what a caller may
/// ask for and not a property of a backend.
///
/// # Errors
///
/// [`IndexErrorKind::InvalidRequest`] for a limit outside `1..=1000`.
pub(crate) fn require_limit(limit: usize) -> Result<(), IndexError> {
    if (1..=MAX_PAGE).contains(&limit) {
        return Ok(());
    }
    Err(IndexError::new(
        IndexErrorKind::InvalidRequest,
        format!("query limit must be between 1 and {MAX_PAGE}"),
        false,
    ))
}

/// Where one history page starts and how long it is.
///
/// Paging is a real second argument to [`History::history`](crate::History),
/// not a property of the subject: a position and a limit belong to *this*
/// request, and two callers paging the same holding are at different positions.
///
/// No [`Default`]: the one it derived was `limit: 0`, which is a page no store
/// answers, so the only ways to build one are the two constructors below and
/// both take the length the caller wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// Resume after this position, or start from the oldest transaction.
    pub after: Option<HistoryPosition>,
    /// Transactions per page, 1..=1000.
    pub limit: usize,
}

impl Page {
    /// The first page of up to `limit` transactions.
    #[must_use]
    pub const fn first(limit: usize) -> Self {
        Self { after: None, limit }
    }

    /// Up to `limit` transactions after `position`.
    #[must_use]
    pub const fn after(position: HistoryPosition, limit: usize) -> Self {
        Self {
            after: Some(position),
            limit,
        }
    }
}

/// A transaction's place in an address's history:
/// `(height, transaction order, transaction id)`.
///
/// This is also where the next [`HistoryQuery`] page starts: a page resumes
/// after the last position it returned, and nothing else. The confirmed store
/// is append-only, so a checkpoint that moved can only have appended *above*
/// that position — an ascending keyset page is stable however long a caller
/// holds it, with no checkpoint to compare and no page to restart
/// (`docs/design/030 §8 (ex-025)` section 6.3).
///
/// **All three components are functions of the block**, which is what keeps
/// that true when more of an already-stored block is written later:
/// [`CanonicalTransaction::transaction_order`] is the transaction's native
/// place in its block rather than a rank over whatever selection reached the
/// block first, so one transaction resolves to one position under every
/// selection and no later watch can renumber rows beneath a held cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPosition {
    /// Height of the including block.
    pub height: BlockHeight,
    /// The transaction's native place in that block.
    pub transaction_order: u64,
    /// The transaction at that place.
    pub transaction: TransactionRef,
}

/// One page of `address`'s canonical history in one `asset`.
///
/// A transaction is on the page iff it is canonical at the page checkpoint and
/// - it has a movement of `asset` whose `from` or `to` is `address`, or
/// - `address` paid its fee and either it failed or `asset` is the fee asset.
///
/// So a failed transaction `address` paid for appears in the history of every
/// asset, and a successful one only in the fee asset's history unless it also
/// moved `asset` for `address`. Each returned transaction carries its fee, which
/// is always the chain's native asset, and **the movements of `asset` that
/// `address` is an endpoint of** — each with both of its own endpoints named,
/// so the counterparty is still there.
///
/// It used to carry every movement of `asset` the transaction made, `address`
/// endpoint or not, which was an artifact of how the rows were stored rather
/// than a decision: one copy of every movement was written under every
/// participating address, including a fee payer that took part in none of them.
/// A balance change is now stored once, for the address it changed, so a page
/// answers about `address` and the transaction is where the rest of the
/// transaction lives.
///
/// Order is ascending `(height, transaction order, transaction id)`, which is
/// native block order. `limit` (1..=1000) counts
/// returned transactions only, so every page but the last is full; `next` is
/// `Some` iff at least one more transaction matches. `after` is a position in
/// the address's history, valid for any asset of the same address. The query
/// network is `address.network`, and `asset.chain` must be its chain
/// (`InvalidRequest`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryQuery {
    /// Whose history; its network is the query network.
    pub address: Address,
    /// Which asset's history; must be of `address`'s chain.
    pub asset: AssetId,
    /// Resume after this position, or start from the oldest transaction.
    pub after: Option<HistoryPosition>,
    /// Transactions per page, 1..=1000.
    pub limit: usize,
}

impl Holder for HistoryQuery {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &AssetId {
        &self.asset
    }

    /// The holding, and the page this request adds to it.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] for a blank address, an asset of
    /// another chain, or a `limit` outside `1..=1000`.
    fn validate(&self) -> Result<(), IndexError> {
        require_holding(&self.address, &self.asset)?;
        require_limit(self.limit)
    }
}

impl HistoryQuery {
    /// The store query for one page of `of`'s history.
    ///
    /// The two halves come from their own owners: the subject's address and
    /// asset from the holder, the position and limit from the request.
    #[must_use]
    pub fn new(of: &dyn Holder, page: Page) -> Self {
        Self {
            address: of.address().clone(),
            asset: of.asset().clone(),
            after: page.after,
            limit: page.limit,
        }
    }

    /// `transaction` as this query returns it: `None` when it does not match,
    /// otherwise with only its movements of the query asset.
    #[must_use]
    pub fn select(&self, mut transaction: CanonicalTransaction) -> Option<CanonicalTransaction> {
        let moved = transaction
            .movements
            .iter()
            .any(|movement| self.moves(movement));
        if !moved && !self.charges(&transaction) {
            return None;
        }
        transaction
            .movements
            .retain(|movement| self.moves(movement));
        Some(transaction)
    }

    /// Whether `movement` moves the query asset to or from the query address.
    fn moves(&self, movement: &ValueMovement) -> bool {
        let endpoint = |address: Option<&Address>| address == Some(&self.address);
        movement.asset == self.asset && (endpoint(movement.from()) || endpoint(movement.to()))
    }

    /// Whether the query address paid `transaction`'s fee and the fee alone
    /// puts it on the page: it failed, or the query asset is the fee asset.
    fn charges(&self, transaction: &CanonicalTransaction) -> bool {
        let Some(fee) = &transaction.fee else {
            return false;
        };
        let failed = matches!(transaction.status, CanonicalStatus::Failed { .. });
        fee.payer.as_ref() == Some(&self.address) && (failed || fee.asset == self.asset)
    }
}

/// A [`ConfirmedHistory`] page: canonical transactions without read-time
/// status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalPage {
    /// The checkpoint the page was read at.
    pub checkpoint: Option<BlockRef>,
    /// Matching transactions, ascending by
    /// `(height, transaction order, transaction id)` — native block order.
    pub transactions: Vec<CanonicalTransaction>,
    /// Where the next page resumes, or `None` when nothing else matches.
    pub next: Option<HistoryPosition>,
    /// How much of the history this page is a window onto the store can prove
    /// it holds.
    ///
    /// **A claim about the stream, not about the window.** The rows on this
    /// page are the rows on this page; saying they are all here would be a
    /// statement no read could ever contradict. What is worth claiming, and
    /// what a caller paging an address's history actually wants to know, is
    /// whether *walking every page* enumerates every transaction the address
    /// ever took part in. So this is the same question [`crate::Balance`]
    /// answers about a fold, asked of the same rows and answered from the same
    /// two facts on the observation's own row — the birthday, and how far has
    /// actually been inspected for it — which is why one store method answers
    /// it for both reads and neither read can be authoritative while the other
    /// is not.
    ///
    /// Only the range claims are reachable here.
    /// [`Completeness::Overdrawn`](crate::Completeness::Overdrawn) is a proof
    /// the *arithmetic* produced and a page does no arithmetic, and
    /// [`Completeness::VerifiedOpening`](crate::Completeness::VerifiedOpening)
    /// is evidence about a total, which a list of transactions is not — and
    /// that half is enforced rather than asked for, by the narrowing
    /// [`crate::History`] takes on its way out
    /// ([`without_a_reported_amount`](crate::Completeness::without_a_reported_amount)).
    pub completeness: Completeness,
}

/// A [`History`] page: matching transactions with confirmation status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionPage {
    /// The confirmed line the page was read at.
    pub checkpoint: Option<BlockRef>,
    /// Matching confirmed transactions, ascending by `(height, transaction
    /// order, transaction id)`. Append-only: nothing already on a page is ever
    /// removed or reordered.
    pub transactions: Vec<ObservedTransaction>,
    /// Matching transactions in blocks above the confirmed line, oldest
    /// first. Populated on the first page only (`after` absent), and bounded
    /// by the chain's confirmation depth.
    pub pending: Vec<ObservedTransaction>,
    /// Where the next page resumes, or `None` when nothing else matches.
    pub next: Option<HistoryPosition>,
    /// How much of the history behind this page the index can prove it holds.
    ///
    /// The same claim, in the same words, that [`crate::Balance`] makes about
    /// a fold, and for the same reason: a page is a read of the rows a
    /// backfill is still inserting *below* it, so `next` being `None` says
    /// nothing about the rows older than the first one here. Without it a
    /// caller paging to the end mid-backfill would hold what reads as a
    /// complete export and is not one (`docs/design/030 §8 (ex-032)`, consumer reads).
    ///
    /// It is [`CanonicalPage::completeness`] after the index has weakened it by
    /// what only the index knows — the rows still arriving below the line, and
    /// a birthday the store's claim was never checked against — exactly as a
    /// balance's is. A value assembled here instead of answered by the store
    /// would be a constant, and a gate over a constant is decoration.
    pub completeness: Completeness,
}

/// One holding's confirmed history, as the repository that holds the rows
/// answers it.
///
/// **A repository's own query API, not a store port.** It is absent from
/// [`crate::Repository`], no chain requires it to index, and nothing routes a
/// read through an indexer to reach it: a store that offers it is a store a
/// consumer may query directly, and one that does not is still a complete sink.
///
/// What it owes beyond the rows is the consistent read
/// [`crate::IndexStore`] states: the page and the checkpoint it reports come
/// from one snapshot, so the line a caller narrows by is the line the rows were
/// read at.
pub trait ConfirmedHistory: Send + Sync {
    /// One checkpoint-consistent page of `query`.
    fn history<'a>(
        &'a self,
        query: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>>;
}

/// So a repository that is neither cheap to clone nor clonable at all is held
/// as `Arc<MyStore>`, which is exactly how [`crate::WatchStore`] and
/// [`crate::IndexStore`] are already reached.
impl<T: ConfirmedHistory + ?Sized> ConfirmedHistory for std::sync::Arc<T> {
    fn history<'a>(
        &'a self,
        query: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        (**self).history(query)
    }
}

/// Reads one holding's transaction history, confirmed and pending.
///
/// The narrow capability every history consumer declares — `apps/api`'s route
/// and Solana's absence proof both hold one `Arc<dyn History>` — and the one
/// thing a [`ConfirmedHistory`] repository cannot satisfy by itself, because
/// `pending` is an index's own memory above the confirmed line.
/// [`crate::ObservedHistory`] is what satisfies it.
pub trait History: Send + Sync {
    /// One `page` of `of`'s history in `of`'s own asset.
    ///
    /// `transactions` is confirmed and append-only, so a cursor into it stays
    /// valid however far the checkpoint moves. `pending` carries the
    /// uncommitted transactions and is populated on the first page only.
    fn history<'a>(
        &'a self,
        of: &'a dyn Holder,
        page: Page,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;
}

/// Value in blocks above the confirmed line, for one address and one asset.
///
/// The two directions are reported apart rather than netted: a payments
/// operator reads "3.0 arriving, 1.0 leaving" as two facts, and
/// [`Decimal`](crate::Decimal) has no subtraction to net them with. Neither
/// side is ever negative.
///
/// Network fees are not folded in. A fee is reported with its transaction in
/// history, and on UTXO chains it is already the difference between the
/// spent inputs and the created outputs, so counting it again would be wrong
/// on one chain or missing on another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingValue {
    /// Value arriving at the address above the line.
    pub incoming: Decimal,
    /// Value leaving the address above the line.
    pub outgoing: Decimal,
    /// The newest block the overlay holds, or `None` when it holds none.
    pub block: Option<BlockRef>,
}

impl PendingValue {
    /// Nothing above the line: what a store's own fold reports, before the
    /// index folds its overlay in.
    #[must_use]
    pub fn none() -> Self {
        Self {
            incoming: Decimal::zero(),
            outgoing: Decimal::zero(),
            block: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;

    fn query(address: &str, limit: usize) -> HistoryQuery {
        let network = base::Network::new(ChainName::new("test"), "mainnet");
        HistoryQuery {
            asset: AssetId::native(network.chain.clone()),
            address: Address::new(network, address),
            after: None,
            limit,
        }
    }

    /// The page bound and the blank address, refused by the request itself.
    ///
    /// Both used to be restated by every store, so two stores could disagree
    /// about what a caller may ask for — and did: one refused a blank address on
    /// an output page and the other answered it with an empty one.
    #[test]
    fn a_query_states_the_page_and_the_address_it_asks_for() {
        query("a", 1).validate().expect("one transaction");
        query("a", MAX_PAGE).validate().expect("the longest page");
        for refused in [query("a", 0), query("a", MAX_PAGE + 1), query("   ", 10)] {
            let error = refused.validate().expect_err("an unanswerable query");
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert!(!error.retryable);
        }
    }
}
