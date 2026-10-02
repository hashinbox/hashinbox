//! What a store may offer beyond [`crate::Repository`], in one file because one
//! rule governs all three: a store that offers a capability owes its whole
//! contract, including the consistent read [`crate::IndexStore`] states.
//!
//! The read vocabulary each one names lives with the queries, not here —
//! [`OutputRequest`] and [`Balance`] in the files beside this one. [`BalanceQuery`]
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
    Address, AssetId, Balance, BlockPosition, BlockRef, BoxFuture, Completeness, Holder,
    IndexError, IndexErrorKind, Network, OutputPage, OutputRequest, WatchProgress,
};

/// How far one address has been inspected, as one snapshot holds it: every
/// observation of the address, and the confirmed line of the same read.
///
/// **What keeps a read honest about rows that are not there yet.** A store
/// answers from the rows it holds, and a backfill exists precisely because some
/// are missing: the range between an observation's recorded progress and the
/// line has not been read for the address. Only the store can say so without a
/// second read, because the observations and the rows are in one snapshot. So
/// each shipped store loads this beside the rows it answers with and asks it
/// what the answer may claim.
///
/// **Scoped to the address, not to the observation**, and deliberately. One
/// inspection of an address serves every asset observed at it
/// ([`crate::BlockInterpreter::inspect`] takes addresses and no asset), so the
/// rows a token read folds are the rows its native sibling's catch-up is still
/// inserting. The conservative answer is the true one.
///
/// Shared by the two shipped stores because it is a rule about
/// [`WatchProgress`] rather than about either backend. A third-party store
/// owes the same answers (`tests/contracts/`); it may ask this for them, or
/// derive them its own way.
pub struct Inspection<'a> {
    observations: &'a [WatchProgress],
    checkpoint: Option<&'a BlockRef>,
}

impl<'a> Inspection<'a> {
    /// `observations` is every observation of one address, whatever its asset,
    /// and `checkpoint` the line of the snapshot they were read in.
    #[must_use]
    pub const fn new(observations: &'a [WatchProgress], checkpoint: Option<&'a BlockRef>) -> Self {
        Self {
            observations,
            checkpoint,
        }
    }

    /// Whether any observation of the address is still reading its own past.
    ///
    /// [`WatchProgress::caught_up`] decides, which is the comparison a pass
    /// selects by: a store and an index cannot disagree about what "still
    /// catching up" means.
    ///
    /// **An observed address at a network with no line has read nothing.** A
    /// pass owes nothing below a line that does not exist, so `caught_up` says
    /// yes there; a read cannot, because no block has been committed for the
    /// network since it was created or purged ([`Purging`]). An empty answer
    /// from that snapshot is the absence of reading, not of rows.
    fn outstanding(&self) -> bool {
        let Some(line) = self.checkpoint else {
            return !self.observations.is_empty();
        };
        self.observations
            .iter()
            .any(|observed| !observed.caught_up(Some(line)))
    }

    /// What the history behind `of`'s rows may claim.
    ///
    /// [`Completeness::CompleteHistory`] needs all of three facts. `of` is
    /// observed from the chain's first block, because the range below a
    /// birthday is one no backfill will ever read. Progress has been recorded
    /// for that observation, because a birthday says where inspection would
    /// start and not that any of it happened. And nothing at the address is
    /// behind the line, because rows that are still arriving prove nothing.
    ///
    /// Never [`Completeness::Overdrawn`]: that is the arithmetic's own proof,
    /// and a caller that holds one keeps it over whatever this answers.
    #[must_use]
    pub fn completeness(&self, of: &dyn Holder) -> Completeness {
        let whole = self
            .observations
            .iter()
            .find(|observed| &observed.watch.asset == of.asset())
            .is_some_and(|own| {
                own.watch.start == BlockPosition(0) && own.indexed_through.is_some()
            });
        if whole && !self.outstanding() {
            return Completeness::CompleteHistory;
        }
        Completeness::Incomplete
    }

    /// Refuses a spendable set while the rows it would be read from are still
    /// arriving.
    ///
    /// **The one read that cannot be weakened instead.** A balance and a
    /// history page carry [`Completeness`], so they answer honestly while an
    /// inspection is outstanding and the caller decides what a partial answer
    /// is worth. An [`OutputPage`] carries no such claim and could not use one:
    /// its consumer is coin selection, which pages the set and spends the
    /// union, so an output offered becomes a transaction input. An unspent row
    /// at block 0 is only unspent *as far as has been read*, and until the
    /// inspection reaches the checkpoint the page is read at, the blocks
    /// between may hold the very spend that settles it. The rows exist, the
    /// checkpoint is real, and the two together are still a lie, which leaves
    /// refusing as the only honest answer.
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::Conflict`], the refusal every caller of
    /// [`OutputStore`] already restarts on. It clears the moment the catch-up
    /// reaches the line, or, at a network with no line yet (never indexed, or
    /// [`Purging`] emptied it), at the first commit.
    pub fn spendable(&self) -> Result<(), IndexError> {
        if !self.outstanding() {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::Conflict,
            "spendable outputs are not readable while this address's own history \
             is still being inspected: an unspent row cannot be proven unspent \
             below the checkpoint it was read at",
            true,
        ))
    }
}

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
/// up.
///
/// The returned [`Balance`] carries the store's own checkpoint as
/// `observed_at` and nothing pending; the blocks above the confirmed line are
/// an index's ([`crate::Pending`]), and a consumer that wants them folds them
/// in.
///
/// [`Balance::completeness`] is this store's own answer, from the snapshot the
/// fold was read in: a holding whose address is still being inspected below
/// `observed_at` is never [`Completeness::CompleteHistory`].
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
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::Conflict`] while any observation of the
    /// address is still being inspected below the checkpoint the page would be
    /// read at, or the address is observed and its network has no checkpoint,
    /// because a row cannot be proven unspent over blocks nothing has read. The
    /// same kind when the checkpoint moved under a cursor.
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>>;
}

/// The same forwarding impl, for the same reason.
impl<T: OutputStore + ?Sized> OutputStore for std::sync::Arc<T> {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        (**self).list(request)
    }
}

/// Taking one network's index back to nothing: offline, or online while that
/// network is quiesced.
///
/// Deliberately **not** part of [`crate::Repository`]: it is the rescan, not
/// indexing, and a third-party sink that does not offer one still indexes
/// correctly.
pub trait Purging: Send + Sync {
    /// Atomically removes every indexing-owned row of exactly one network —
    /// its checkpoint, history, movements, block credits, and live outputs —
    /// and reports how many of each it removed.
    ///
    /// The rescan, and the only way back from an
    /// [`crate::IndexErrorKind::ConfirmedReorg`].
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
    /// **Offline, or online by the network's controller.** Online, the caller
    /// owes the quiesced window: admissions for the network are blocked (the
    /// application's), its unit is paused (`SyncControl::pause` has returned,
    /// so no pass is mid-commit), and before resuming it the caller resets that
    /// network's indexer ([`crate::Indexing::reset`]). The next pass then
    /// re-derives every row from the registered birthdays.
    ///
    /// **Reads need no gate of the controller's: the purged rows answer for
    /// themselves.** From this purge until the first commit after resume, the
    /// network has no checkpoint and no observation has progress, and that is
    /// what a read reports rather than an authoritative empty history: a
    /// balance or history page is [`crate::Completeness::Incomplete`] with no
    /// checkpoint, and an observed address's spendable set is refused with a
    /// retryable [`crate::IndexErrorKind::Conflict`] ([`Inspection`]). Once
    /// passes commit again, each read is as of the lower checkpoint the rescan
    /// has reached, under the rule every catch-up reads by. A third-party store
    /// offering this port owes the same answers.
    ///
    /// **Done once per window.** A second purge removes nothing, and a process
    /// restarted between the purge and the resume needs none: it loads the
    /// same observations without progress and scans from their birthdays.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::Store`] when the removal itself fails.
    fn purge_network<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<PurgedRows, IndexError>>;
}

/// Rows removed by one network rescan, counted by stored collection.
/// Stores with a separate movement collection report its count; stores nesting
/// movements inside transactions remove them with their history rows.
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
