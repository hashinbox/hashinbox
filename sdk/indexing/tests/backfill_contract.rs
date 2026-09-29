//! Step 12 of `docs/design/030`: an address registered after its history began
//! ends up with that history, and nothing claims to be complete while it is
//! still arriving.
//!
//! The fixture is deliberately the shape that used to lose data silently. Ten
//! blocks are indexed with nothing watched at all, so the store holds a
//! checkpoint and no history; then an address is watched from a position the
//! network passed long ago. Before this step that watch indexed exactly
//! nothing and said so nowhere.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use base::Decimal;
use futures_executor::block_on;
use indexing::{
    Address, AssetId, Balance, BalanceQuery, BalanceStore, Balances, BlockHash, BlockHeight,
    BlockInterpreter, BlockParent, BlockPosition, BlockRef, BlockSource, BoxFuture, CanonicalPage,
    ChainName, Completeness, ConfirmedHistory, FeeSettlement, HistoryQuery, IndexError,
    IndexErrorKind, IndexStore, IndexedBatch, IndexedBlock, InterpretedBlock, MovementId, Network,
    ObservationDraft, ObservationDraftStatus, OutputChanges, OutputKey, Page, PurgedRows, Purging,
    Service, Start, Subject, SyncConfig, SyncPhase, SyncPolicy, SyncStatus, TransactionRef,
    ValueMovement, Watching, memory,
};

/// Every block above `pruned` exists, and the source counts what a pass asked
/// it for.
#[derive(Clone, Default)]
struct Source {
    tip: u64,
    /// The oldest block this node still holds. A request below it is refused
    /// the way a pruned node refuses one, and never stops refusing.
    pruned: u64,
    /// One produced block this node leaves out of every range it serves, while
    /// answering for every other one — the shape a claim must never be built
    /// over, because the block it omits is one nothing will ever inspect again.
    skipped: Option<u64>,
    fetched: Arc<AtomicUsize>,
}

impl Source {
    /// The same node, having produced up to `tip`, counting into the same
    /// total so a test can measure one restart against another.
    fn at(&self, tip: u64) -> Self {
        Self {
            tip,
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug)]
struct Block(BlockRef);

impl IndexedBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

fn network() -> Network {
    Network::new(ChainName::new("backfilled"), "testnet")
}

fn asset() -> AssetId {
    AssetId::native(network().chain)
}

fn subject(value: &str) -> Subject {
    Subject {
        address: Address::new(network(), value),
        asset: asset(),
    }
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(height.to_be_bytes().to_vec()),
        parent: height.checked_sub(1).map(|parent| BlockParent {
            position: BlockPosition(parent),
            hash: BlockHash(parent.to_be_bytes().to_vec()),
        }),
        timestamp: None,
    }
}

impl indexing::ChainTip for Source {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, indexing::SourceError>> {
        Box::pin(async move { Ok(block(self.tip)) })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, indexing::SourceError>> {
        Box::pin(async move { Ok(block(self.tip)) })
    }
}

impl BlockSource for Source {
    type Block = Block;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, indexing::SourceError>> {
        if start.0 < self.pruned {
            return Box::pin(async move {
                Err(indexing::SourceError {
                    message: "block is not available on this node".into(),
                    retryable: true,
                })
            });
        }
        let blocks: Vec<_> = (start.0..=end.0.min(self.tip))
            .filter(|height| Some(*height) != self.skipped)
            .take(limit)
            .map(|height| Block(block(height)))
            .collect();
        self.fetched.fetch_add(blocks.len(), Ordering::Relaxed);
        Box::pin(async move { Ok(blocks) })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, indexing::SourceError>> {
        let found = (position.0 <= self.tip).then(|| block(position.0));
        Box::pin(async move { Ok(found) })
    }
}

/// Credits every selected address in every block, and nothing otherwise: a
/// block indexed with an empty selection produces no row at all, which is what
/// leaves the gap this test closes.
///
/// It also records what it was asked, block by block, because the selection *is*
/// the subject of one scenario here: which observations a given block was
/// inspected for is invisible from the store afterwards.
#[derive(Clone, Default)]
struct Interpreter {
    inspected: Arc<std::sync::Mutex<Vec<Inspection>>>,
}

/// One block, and the addresses it was inspected for.
type Inspection = (u64, Vec<String>);

impl Interpreter {
    /// Every `(block, addresses)` pair this interpreter was handed, in order.
    fn calls(&self) -> Vec<Inspection> {
        self.inspected.lock().expect("inspections").clone()
    }
}

impl BlockInterpreter for Interpreter {
    type Block = Block;

    fn settlement(&self) -> FeeSettlement {
        FeeSettlement::Charged
    }

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        let reference = block.block_ref();
        self.inspected.lock().expect("inspections").push((
            reference.height.0,
            addresses.iter().map(|at| at.value.clone()).collect(),
        ));
        // Every address gets its own transaction in the same block, so the
        // ordinal is the address's place in the block's whole list rather than
        // in the selection — which is what the interpreters supply.
        let transactions = (0_u64..)
            .zip(addresses)
            .map(|(order, address)| ObservationDraft {
                transaction_id: TransactionRef::new(
                    network(),
                    format!("{}-{}", address.value, reference.height.0),
                ),
                transaction_order: order,
                status: ObservationDraftStatus::Included,
                movements: vec![ValueMovement::transfer(
                    MovementId("0".into()),
                    asset(),
                    Decimal::from(10_u64),
                    Address::new(network(), "treasury"),
                    address.clone(),
                )],
                fee: None,
            })
            .collect();
        Ok(InterpretedBlock {
            block: reference,
            transactions,
            outputs: OutputChanges::default(),
            credits: Vec::new(),
        })
    }
}

/// The in-memory store with one answer changed: its fold always claims
/// whatever completeness it was built with.
///
/// Two gates are checked through it and they are different gates, which is why
/// the claim is a field rather than a constant. A store claiming
/// [`Completeness::VerifiedOpening`] — evidence about the *total* at one block —
/// tests the backfill gate, which weakens any claim while an address's rows are
/// still arriving. A store claiming [`Completeness::CompleteHistory`] —
/// evidence about the *range* — tests the birthday gate, which refuses that
/// claim outright for an observation that began above the chain's first block.
/// Nothing in the tree produces either yet, so without this the gates would be
/// tested vacuously.
#[derive(Clone)]
struct Optimistic(memory::Repository, Completeness);

impl BalanceStore for Optimistic {
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            let mut balance = self.0.balance(query).await?;
            balance.completeness = self.1;
            Ok(balance)
        })
    }
}

impl IndexStore for Optimistic {
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

impl Purging for Optimistic {
    fn purge_network<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        self.0.purge_network(network)
    }
}

impl ConfirmedHistory for Optimistic {
    fn history<'a>(
        &'a self,
        query: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        self.0.history(query)
    }
}

impl indexing::WatchStore for Optimistic {
    fn add<'a>(&'a self, watches: Vec<indexing::Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        self.0.add(watches)
    }

    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<indexing::WatchProgress>, IndexError>> {
        self.0.all(network)
    }

    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a indexing::Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        self.0.forget(network, watch)
    }

    fn reanchor<'a>(
        &'a self,
        watches: Vec<indexing::Watch>,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        self.0.reanchor(watches)
    }
}

type Index<R> = Service<Source, Interpreter, R>;

fn service<R: Clone>(repository: R, source: Source, policy: SyncPolicy) -> Arc<Index<R>> {
    watching(repository, source, policy, Interpreter::default())
}

/// The same, over an interpreter the caller keeps, so a scenario can read which
/// observations each block was inspected for.
fn watching<R: Clone>(
    repository: R,
    source: Source,
    policy: SyncPolicy,
    interpreter: Interpreter,
) -> Arc<Index<R>> {
    Arc::new(Service::new(
        source,
        interpreter,
        repository,
        SyncConfig::new(&network(), policy).expect("sync config"),
    ))
}

fn memory_service(
    memory: &memory::Repository,
    source: Source,
    policy: SyncPolicy,
) -> Arc<Index<memory::Repository>> {
    service(memory.clone(), source, policy)
}

/// One pass, returning the status of the only network.
fn pass<R>(index: &Arc<Index<R>>) -> SyncStatus
where
    Index<R>: indexing::Indexing,
{
    block_on(indexing::Indexing::sync(&**index)).expect("pass")
}

/// What a consumer reads: the reader over this index, which joins the
/// repository's own confirmed page to the blocks the index still holds.
fn indexed<R>(index: &Arc<Index<R>>, address: &str) -> Vec<String>
where
    R: indexing::ConfirmedHistory + Clone,
{
    let reader = index.history();
    block_on(indexing::History::history(
        &reader,
        &subject(address),
        Page::first(100),
    ))
    .expect("history")
    .transactions
    .into_iter()
    .map(|entry| entry.transaction_id.id.to_string())
    .collect()
}

/// Ten blocks indexed with nothing watched, so the store holds a checkpoint
/// and no history at all.
fn caught_up(memory: &memory::Repository, source: Source) -> Arc<Index<memory::Repository>> {
    let index = memory_service(memory, source, SyncPolicy::new(100));
    assert_eq!(pass(&index).phase, SyncPhase::Ready);
    index
}

fn source(tip: u64) -> Source {
    Source {
        tip,
        pruned: 0,
        skipped: None,
        fetched: Arc::new(AtomicUsize::new(0)),
    }
}

/// The property: a watch below the checkpoint is honoured, not discarded.
#[test]
fn an_address_watched_from_its_own_past_ends_up_with_that_history() {
    let memory = memory::Repository::new();
    let index = caught_up(&memory, source(10));
    let late = subject("late");

    block_on(index.watch("late-1", &late, Start::At(BlockPosition(2)))).expect("watch");
    assert!(indexed(&index, "late").is_empty(), "nothing is indexed yet");

    let status = pass(&index);

    assert_eq!(
        indexed(&index, "late").len(),
        9,
        "blocks 2 through 10 are indexed for an address watched at 2"
    );
    assert_eq!(status.checkpoint, Some(block(10)), "nothing moved forward");
    assert!(status.backfill.is_none(), "one pass closed the whole gap");
    assert_eq!(status.phase, SyncPhase::Ready);
}

/// An address watched from *now* owes nothing and is never swept.
#[test]
fn a_fresh_watch_owes_nothing_and_reads_no_historical_block() {
    let memory = memory::Repository::new();
    let source = source(10);
    let index = caught_up(&memory, source.clone());
    let read = source.fetched.load(Ordering::Relaxed);

    block_on(index.watch("fresh-1", &subject("fresh"), Start::Fresh)).expect("watch");
    let status = pass(&index);

    assert!(status.backfill.is_none(), "a fresh watch owes nothing");
    assert_eq!(
        source.fetched.load(Ordering::Relaxed),
        read,
        "and so costs no block fetch at all"
    );
}

/// A balance folded over a range still being filled in does not claim to be
/// complete, whatever the store claims.
#[test]
fn a_balance_over_an_incomplete_range_never_claims_complete() {
    let memory = memory::Repository::new();
    let repository = Optimistic(memory.clone(), Completeness::VerifiedOpening);
    let index = service(repository, source(10), SyncPolicy::new(100));
    assert_eq!(pass(&index).phase, SyncPhase::Ready);

    let late = subject("late");
    block_on(index.watch("late-1", &late, Start::At(BlockPosition(2)))).expect("watch");

    let during = block_on(index.balances().balance(&late)).expect("balance mid-backfill");
    assert_eq!(
        during.completeness,
        Completeness::Incomplete,
        "the store claimed Complete; the index will not pass it on while it \
         is still writing this address's rows"
    );

    assert!(pass(&index).backfill.is_none(), "the gap closes");

    let after = block_on(index.balances().balance(&late)).expect("balance after");
    assert_eq!(
        after.completeness,
        Completeness::VerifiedOpening,
        "once nothing is owed the gate stops applying"
    );
    assert_eq!(after.amount, Decimal::from(90_u64), "nine credits of ten");
}

/// The token observed at an address reads as incomplete while the *native*
/// observation of the same address is still reading its own past.
///
/// The gate is address-scoped rather than per observation, and that is the
/// honest answer rather than a looser one: one inspection of an address serves
/// every asset observed at it (`indexing::BlockInterpreter::inspect` takes
/// addresses and no asset), so the rows a token fold reads are the rows the
/// native catch-up is still inserting. A gate keyed by the observation would
/// call the token holding authoritative out of rows that are still arriving.
///
/// Both ends of the scenario read `Complete`, which is what stops the middle
/// assertion from passing for the trivial reason that nothing ever claims it:
/// the store here claims `Complete` always, and the only thing that changes
/// between the three reads is what the address owes.
#[test]
fn a_token_read_is_weakened_while_its_native_sibling_catches_up() {
    let memory = memory::Repository::new();
    let index = service(
        Optimistic(memory, Completeness::VerifiedOpening),
        source(10),
        SyncPolicy::new(100),
    );
    assert_eq!(pass(&index).phase, SyncPhase::Ready);
    let late = subject("late");
    let token = Subject {
        address: Address::new(network(), "late"),
        asset: AssetId {
            chain: network().chain,
            asset: "token".into(),
        },
    };

    // Born above the line, so this observation owes nothing at all.
    block_on(index.watch("late-token", &token, Start::Fresh)).expect("the token observation");
    assert_eq!(
        block_on(index.balances().balance(&token))
            .expect("the token balance")
            .completeness,
        Completeness::VerifiedOpening,
        "nothing is owed at this address yet, so nothing weakens the claim"
    );

    // A second observation of the same address, born below the line.
    block_on(index.watch("late-native", &late, Start::At(BlockPosition(2))))
        .expect("the native observation");

    assert_eq!(
        block_on(index.balances().balance(&token))
            .expect("the token balance mid-backfill")
            .completeness,
        Completeness::Incomplete,
        "the token owes nothing of its own, and the rows it folds are the ones \
         the native catch-up at the same address is still inserting"
    );

    assert!(pass(&index).backfill.is_none(), "the native range closes");

    assert_eq!(
        block_on(index.balances().balance(&token))
            .expect("the token balance after")
            .completeness,
        Completeness::VerifiedOpening,
        "and the gate stops applying once the address owes nothing again"
    );
}

/// **The obligation `034` left open, now satisfied — and the reason the two
/// completeness claims had to be named apart.**
///
/// The obligation is *honest balance completeness with arbitrary birthdays*: a
/// partial total must never be presented as authoritative. This fixture is the
/// shape that used to break it — every block from 0 credits the address, the
/// observation starts at 2, so blocks 0 and 1 are credits the index will never
/// read, and the fold is short by exactly them. Once the range *above* the
/// birthday closes nothing weakens the claim any more, so a store's claim
/// reached the caller over a total that was provably incomplete.
///
/// It was open because one name covered two claims. `Complete` was documented
/// as "the start is proven to be at or before the address's first use" while
/// the only thing that ever produced it — a node read agreeing with the fold at
/// one block — proves the *total* there and nothing about the start. There was
/// therefore no claim for the index to check, because the claim it would have
/// had to refuse was the same value the one real producer emitted.
///
/// Splitting them gives the index something checkable.
/// [`Completeness::CompleteHistory`] is a statement about the *range*, and the
/// index holds the one fact that bounds it: the observation's own birthday. A
/// whole-history claim over a birthday above the chain's first block is refused
/// — not weakened until the backfill finishes, refused, because the range below
/// a birthday is one no backfill will ever read.
/// [`Completeness::VerifiedOpening`] is untouched by that rule, which is what
/// keeps the assertion in
/// `a_balance_over_an_incomplete_range_never_claims_complete` above true and
/// unchanged: that gate is about rows not read *yet*, this one about rows that
/// will never be read at all, and they are different gates over different
/// claims.
#[test]
fn a_complete_claim_is_checked_against_the_birthday_it_was_earned_under() {
    let memory = memory::Repository::new();
    let index = service(
        Optimistic(memory, Completeness::CompleteHistory),
        source(10),
        SyncPolicy::new(100),
    );
    assert_eq!(pass(&index).phase, SyncPhase::Ready);
    let late = subject("late");

    block_on(index.watch("late-1", &late, Start::At(BlockPosition(2)))).expect("watch");
    assert!(pass(&index).backfill.is_none(), "the range above 2 closes");

    let balance = block_on(index.balances().balance(&late)).expect("balance");
    assert_eq!(
        balance.amount,
        Decimal::from(90_u64),
        "nine credits of ten, and two below the birthday that were never read"
    );
    assert_eq!(
        balance.completeness,
        Completeness::Incomplete,
        "a total short by every pre-birthday credit must not read as complete"
    );
}

/// The other half of the same gate: an observation that *did* start at the
/// chain's first block keeps the claim, so the refusal above is a check and not
/// a blanket refusal.
///
/// Without this the gate would pass by never letting anything through, which is
/// the failure mode a one-sided assertion cannot tell from a working check.
#[test]
fn a_complete_claim_survives_a_birthday_at_the_first_block() {
    let memory = memory::Repository::new();
    let index = service(
        Optimistic(memory, Completeness::CompleteHistory),
        source(10),
        SyncPolicy::new(100),
    );
    assert_eq!(pass(&index).phase, SyncPhase::Ready);
    let early = subject("early");

    block_on(index.watch("early-1", &early, Start::At(BlockPosition(0)))).expect("watch");
    assert!(pass(&index).backfill.is_none(), "the whole range closes");

    assert_eq!(
        block_on(index.balances().balance(&early))
            .expect("balance")
            .completeness,
        Completeness::CompleteHistory,
        "every block the address could have acted in was inspected for it",
    );
}

/// A pass reads no more than its batch size, and what it leaves it reports.
#[test]
fn one_pass_reads_no_more_blocks_than_its_batch_size() {
    let memory = memory::Repository::new();
    let source = source(10);
    let index = caught_up(&memory, source.clone());
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0)))).expect("watch");

    let bounded = memory_service(&memory, source.clone(), SyncPolicy::new(3));
    let read = source.fetched.load(Ordering::Relaxed);
    let status = pass(&bounded);

    assert_eq!(
        source.fetched.load(Ordering::Relaxed) - read,
        3,
        "the backfill reads the batch size and stops"
    );
    let backfill = status.backfill.expect("eight blocks are still owed");
    assert_eq!(backfill.pending, 1);
    assert_eq!(backfill.inspected, 3);
    assert_eq!(backfill.resume, BlockPosition(3));
    assert_eq!(backfill.anchor, BlockPosition(10));
    assert_eq!(indexed(&bounded, "late").len(), 3);
}

/// A backfill interrupted by a restart resumes where it stopped and reaches
/// the same end state as one that ran straight through.
#[test]
fn an_interrupted_backfill_reaches_the_same_end_state() {
    let uninterrupted = {
        let memory = memory::Repository::new();
        let source = source(10);
        let index = caught_up(&memory, source.clone());
        block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0))))
            .expect("watch");
        let straight = memory_service(&memory, source, SyncPolicy::new(100));
        assert!(pass(&straight).backfill.is_none());
        indexed(&straight, "late")
    };

    let memory = memory::Repository::new();
    let source = source(10);
    let index = caught_up(&memory, source.clone());
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0)))).expect("watch");

    // Three blocks, then the process dies: a new service over the same store,
    // told nothing by anyone.
    let first = memory_service(&memory, source.clone(), SyncPolicy::new(3));
    assert_eq!(pass(&first).backfill.expect("owed").inspected, 3);
    drop(first);

    let resumed = memory_service(&memory, source.clone(), SyncPolicy::new(100));
    let read = source.fetched.load(Ordering::Relaxed);
    assert!(
        pass(&resumed).backfill.is_none(),
        "the rest of the range closes in one pass"
    );
    assert_eq!(
        source.fetched.load(Ordering::Relaxed) - read,
        8,
        "the resumed pass reads blocks 3 to 10 and never rereads 0 to 2"
    );

    let mut resumed_history = indexed(&resumed, "late");
    let mut expected = uninterrupted;
    resumed_history.sort();
    expected.sort();
    assert_eq!(resumed_history, expected);
    assert_eq!(resumed_history.len(), 11);
}

/// A settled backfill is recorded, so a restart never sweeps the same range
/// again.
#[test]
fn a_settled_backfill_is_not_swept_again_after_a_restart() {
    let memory = memory::Repository::new();
    let source = source(10);
    let index = caught_up(&memory, source.clone());
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(2)))).expect("watch");
    assert!(pass(&index).backfill.is_none());
    drop(index);

    let restarted = memory_service(&memory, source.clone(), SyncPolicy::new(100));
    let read = source.fetched.load(Ordering::Relaxed);
    let status = pass(&restarted);

    assert!(status.backfill.is_none(), "nothing is owed after a restart");
    assert_eq!(
        source.fetched.load(Ordering::Relaxed),
        read,
        "and no block is fetched to find that out"
    );
    assert_eq!(indexed(&restarted, "late").len(), 9);
}

/// A second observation of a watched address, born below what is already
/// recorded for the first, survives a restart taken before any pass.
///
/// **The defect this pins is now structurally impossible, and that is the point
/// of keeping the test.** Progress used to be one row per *address*: registering
/// a second observation from further back lowered that shared row in memory, and
/// only a later pass wrote it down, so a restart in between read the *older,
/// higher* row back and took it at face value — a watch that asked for block 2
/// silently never got 2..6, because the recorded range had already settled, so
/// nothing was outstanding, nothing was fetched, and no read said anything was
/// missing.
///
/// Progress is now one row per *observation*, earned under that observation's own
/// immutable birthday, so there is no shared row to widen and nothing to lose in
/// the gap. What this test still checks from the outside is the behaviour: nine
/// blocks, read rather than assumed.
#[test]
fn a_lower_birthday_registered_before_a_restart_is_not_lost_to_recorded_progress() {
    let memory = memory::Repository::new();
    let source = source(10);
    let index = caught_up(&memory, source.clone());

    // The first observation settles its own range, durably.
    block_on(index.watch("late-native", &subject("late"), Start::At(BlockPosition(7))))
        .expect("the first observation");
    assert!(pass(&index).backfill.is_none(), "7..10 is settled");
    assert_eq!(indexed(&index, "late").len(), 4, "blocks 7 to 10");

    // A second asset at the same address, from further back, and then the
    // process dies before any pass can record the wider claim.
    let token = Subject {
        address: Address::new(network(), "late"),
        asset: AssetId {
            chain: network().chain,
            asset: "token".into(),
        },
    };
    block_on(index.watch("late-token", &token, Start::At(BlockPosition(2))))
        .expect("the second observation");
    drop(index);

    let restarted = memory_service(&memory, source.clone(), SyncPolicy::new(100));
    let read = source.fetched.load(Ordering::Relaxed);

    assert!(
        pass(&restarted).backfill.is_none(),
        "one pass closes the widened range"
    );
    assert!(
        source.fetched.load(Ordering::Relaxed) > read,
        "and it read blocks to do it, rather than settling a range it never saw"
    );
    assert_eq!(
        indexed(&restarted, "late").len(),
        9,
        "blocks 2 to 10, not just the 7 to 10 the first observation claimed"
    );
}

/// **The `docs/design/030 §8 (ex-032)` table, as one test.** Three observations of the same
/// network, in three different states, and one pass over the union of what they
/// owe.
///
/// | Wallet | Birthday | Indexed through | Next inspection |
/// | --- | ---: | ---: | ---: |
/// | X | 20 | 25 | 26 |
/// | Y | 22 | None | 22 |
/// | Z | 10 | 30 | 31 |
///
/// With a confirmed line at 30: scan 22 to 30 — blocks 22 to 25 inspected for Y
/// alone, 26 to 30 for X *and* Y — and Z requires no work at all. Each block is
/// fetched **once** and interpreted once, for the observations that need it.
/// Adding Y resets neither X nor Z, which is the property a shared per-address
/// claim could not give: each observation carries its own contiguous
/// `indexed_through`, earned under its own immutable birthday.
#[test]
fn one_pass_serves_three_observations_from_their_own_positions() {
    let memory = memory::Repository::new();
    let source = source(30);
    // A line at 30 with nothing watched, so every observation below is behind it.
    let index = caught_up(&memory, source.clone());

    // Z, born at 10, reads all the way to the line and joins the front.
    block_on(index.watch("z", &subject("z"), Start::At(BlockPosition(10)))).expect("z");
    assert!(pass(&index).backfill.is_none(), "z closes its whole range");
    let z_history = indexed(&index, "z").len();
    assert_eq!(z_history, 21, "blocks 10 through 30");

    // X, born at 20, reads exactly six blocks and stops: one bounded pass.
    block_on(index.watch("x", &subject("x"), Start::At(BlockPosition(20)))).expect("x");
    let bounded = service(memory.clone(), source.clone(), SyncPolicy::new(6));
    let stopped = pass(&bounded).backfill.expect("x still owes 26 to 30");
    assert_eq!(stopped.inspected, 6, "blocks 20 through 25");
    assert_eq!(stopped.resume, BlockPosition(26), "x resumes at 26");
    assert_eq!(indexed(&bounded, "x").len(), 6);
    drop(bounded);

    // Y, born at 22, has had nothing inspected for it at all.
    block_on(index.watch("y", &subject("y"), Start::At(BlockPosition(22)))).expect("y");
    drop(index);

    let interpreter = Interpreter::default();
    let one = watching(
        memory.clone(),
        source.clone(),
        SyncPolicy::new(100),
        interpreter.clone(),
    );
    let read = source.fetched.load(Ordering::Relaxed);
    assert!(
        pass(&one).backfill.is_none(),
        "one pass closes what x and y owe between them"
    );

    assert_eq!(
        source.fetched.load(Ordering::Relaxed) - read,
        9,
        "blocks 22 through 30, fetched once for the two observations that need \
         them rather than once each"
    );
    assert_eq!(
        interpreter.calls(),
        (22..=30)
            .map(|height| (
                height,
                if height <= 25 {
                    vec!["y".to_owned()]
                } else {
                    vec!["x".to_owned(), "y".to_owned()]
                }
            ))
            .collect::<Vec<_>>(),
        "22 to 25 inspected for y alone, 26 to 30 for x and y, and nothing for z"
    );
    assert_eq!(indexed(&one, "y").len(), 9, "y: blocks 22 through 30");
    assert_eq!(indexed(&one, "x").len(), 11, "x: blocks 20 through 30");
    assert_eq!(
        indexed(&one, "z").len(),
        z_history,
        "and z is untouched: adding y reset neither of the others"
    );
}

/// A start further below the checkpoint than the policy allows is refused by
/// the call that asked for it, naming the earliest start it would accept.
#[test]
fn a_start_below_the_policy_limit_is_refused_synchronously() {
    let memory = memory::Repository::new();
    let index = memory_service(&memory, source(10), SyncPolicy::new(100).with_backfill(4));
    assert_eq!(pass(&index).phase, SyncPhase::Ready);

    let error = block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(2))))
        .expect_err("a start nine blocks below the checkpoint");

    assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
    assert!(!error.retryable, "the caller must choose another start");
    assert!(
        error.message.contains("earliest acceptable start is 6"),
        "the refusal names a start that would work: {}",
        error.message
    );
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(6))))
        .expect("the named start is accepted");
}

/// A watch that owes nothing is never swept, and needs no record saying so.
///
/// What a restart re-derives is `next_required`, out of the observation's own
/// immutable birthday and whatever block was last inspected for it — so a watch
/// born above the line owes nothing whether or not anything has been recorded,
/// and one that has been inspected resumes after the block it names. Nothing is
/// derived against a *moving* checkpoint, which is what used to make an
/// unrecorded answer a different answer after the chain advanced: it swept from
/// the birthday to the current line, over history the forward pass had already
/// indexed as it went, and on a pruned node over a range that cannot be read at
/// all, so startup readiness never arrived.
#[test]
fn a_fresh_watch_owes_nothing_so_a_restart_never_sweeps_it() {
    let memory = memory::Repository::new();
    let source = source(10);
    let index = caught_up(&memory, source.clone());
    block_on(index.watch("fresh-1", &subject("fresh"), Start::Fresh)).expect("watch");
    assert!(
        pass(&index).backfill.is_none(),
        "a fresh watch owes nothing"
    );
    drop(index);

    // The chain moves on and the process restarts. The forward pass indexes
    // the watched address for every block it passes, which is the whole of
    // what that watch asked for, so those ten blocks are the only ones any
    // phase of this pass has a reason to read.
    let moved = source.at(20);
    let ahead = memory_service(&memory, moved.clone(), SyncPolicy::new(100));
    let read = moved.fetched.load(Ordering::Relaxed);
    assert_eq!(pass(&ahead).phase, SyncPhase::Ready);
    assert_eq!(
        moved.fetched.load(Ordering::Relaxed) - read,
        10,
        "blocks 11 through 20, read once by the forward phase and not swept again"
    );
    assert_eq!(indexed(&ahead, "fresh").len(), 10, "blocks 11 through 20");
    drop(ahead);

    let restarted = memory_service(&memory, moved.clone(), SyncPolicy::new(100));
    let read = moved.fetched.load(Ordering::Relaxed);
    let status = pass(&restarted);

    assert!(status.backfill.is_none(), "nothing is owed after a restart");
    assert_eq!(
        moved.fetched.load(Ordering::Relaxed),
        read,
        "and no block is re-read to find that out"
    );
    assert_eq!(indexed(&restarted, "fresh").len(), 10);
}

/// A historical range with a produced block missing from the middle settles
/// nothing, rather than claiming to have inspected it.
///
/// The claim a filling read writes says every block from where an observation
/// resumed through the highest one read *was inspected for it*, and nothing
/// downstream ever re-derives that: an omitted block in between is history the
/// address loses permanently and silently. A block arriving at the tip has always
/// been checked against the one before it; this is the same reading, so it is the
/// same check (`sdk/indexing/src/scan.rs`).
#[test]
fn a_historical_range_missing_a_produced_block_settles_nothing() {
    let memory = memory::Repository::new();
    let index = caught_up(&memory, source(10));
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0)))).expect("watch");
    drop(index);

    // The same node, having lost one block in the middle of the range.
    let gapped = memory_service(
        &memory,
        Source {
            skipped: Some(5),
            ..source(10)
        },
        SyncPolicy::new(100),
    );
    let status = pass(&gapped);

    let backfill = status.backfill.expect("the range is still owed");
    let failure = backfill.failure.expect("and the pass says why");
    assert_eq!(failure.kind, IndexErrorKind::CannotConnect);
    assert!(failure.retryable, "a node may serve the block next time");
    assert_eq!(
        backfill.resume,
        BlockPosition(0),
        "nothing was settled, so the whole range is still owed"
    );
    assert!(
        indexed(&gapped, "late").is_empty(),
        "and no row of the partial reading was written"
    );
    assert_eq!(
        status.phase,
        SyncPhase::Ready,
        "while the tip every client reads is untouched"
    );
}

/// A resumed historical read must follow the block the observation itself last
/// recorded, and not merely be contiguous within one batch.
///
/// The gap here is between two reads rather than inside one: three blocks are
/// settled, then the node stops serving the very next block. Reading from the one
/// after it is internally contiguous and would prove a claim covering a block
/// nobody inspected — and `IndexedBatch::new` cannot catch it, because a claim
/// naming a block the batch carries says nothing about where the observation
/// resumed from.
#[test]
fn a_resumed_historical_read_must_follow_what_the_observation_recorded() {
    let memory = memory::Repository::new();
    let index = caught_up(&memory, source(10));
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0)))).expect("watch");
    let bounded = memory_service(&memory, source(10), SyncPolicy::new(3));
    assert_eq!(
        pass(&bounded).backfill.expect("owed").resume,
        BlockPosition(3)
    );
    drop(bounded);

    let gapped = memory_service(
        &memory,
        Source {
            skipped: Some(3),
            ..source(10)
        },
        SyncPolicy::new(100),
    );
    let status = pass(&gapped);

    let backfill = status
        .backfill
        .expect("the rest of the range is still owed");
    assert_eq!(
        backfill.failure.expect("and the pass says why").kind,
        IndexErrorKind::CannotConnect
    );
    assert_eq!(
        backfill.resume,
        BlockPosition(3),
        "the observation still resumes at the block it never read"
    );
    assert_eq!(
        indexed(&gapped, "late").len(),
        3,
        "and it holds exactly the three blocks it did read"
    );
}

/// A node that has pruned the range a watch asked for: the pass reports the
/// refusal instead of passing it off as productive catch-up.
///
/// Forward progress is untouched — that is what every client reads — but the
/// historical read got nowhere, and a status that did not say so would have
/// the supervisor reset its attempt count and ask again at once, forever.
#[test]
fn a_backfill_the_source_cannot_serve_reports_its_failure() {
    let memory = memory::Repository::new();
    let pruned = Source {
        pruned: 5,
        ..source(10)
    };
    let index = caught_up(&memory, pruned.clone());
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0)))).expect("watch");

    let status = pass(&index);

    let backfill = status.backfill.expect("the range is still owed");
    let failure = backfill
        .failure
        .expect("and the pass says why it did not read it");
    assert_eq!(failure.kind, IndexErrorKind::Source);
    assert!(failure.retryable);
    assert_eq!(backfill.inspected, 0, "nothing was read");
    assert_eq!(
        status.checkpoint,
        Some(block(10)),
        "and forward progress is untouched"
    );
}

/// The availability floor (`docs/design/024` section 7): a historical range the
/// node will never serve must not make the network unanswerable.
///
/// A pruned range is a property of one watch's request, not of the chain. The
/// checkpoint is at the tip, every block above the birthday is indexed, and
/// every read that the missing range weakens already says so through
/// [`Completeness`]. Reporting `CatchingUp` for a range that can never close
/// held `/health/ready` at 503 for the life of the process — and, for a
/// wallet configured before the supervisor starts, stopped the HTTP listener
/// binding at all. So the phase follows forward progress, and the owed range
/// is reported beside it rather than by withholding the whole API.
#[test]
fn a_backfill_the_source_cannot_serve_still_answers_for_the_tip() {
    let memory = memory::Repository::new();
    let pruned = Source {
        pruned: 5,
        ..source(10)
    };
    let index = caught_up(&memory, pruned);
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0)))).expect("watch");

    let status = pass(&index);

    assert_eq!(
        status.phase,
        SyncPhase::Ready,
        "forward indexing reached the tip, so the network answers for it"
    );
    let backfill = status
        .backfill
        .expect("and the owed range is still reported");
    assert!(
        backfill.failure.is_some(),
        "with the reason an operator has to read"
    );
    assert_eq!(backfill.resume, BlockPosition(0));
    assert_eq!(backfill.anchor, BlockPosition(10));
}

/// A range that is merely unfinished is the same decision: the tip is served,
/// and the gap is reported rather than withheld.
#[test]
fn an_unfinished_backfill_does_not_withhold_the_tip() {
    let memory = memory::Repository::new();
    let source = source(10);
    let index = caught_up(&memory, source.clone());
    block_on(index.watch("late-1", &subject("late"), Start::At(BlockPosition(0)))).expect("watch");

    let bounded = memory_service(&memory, source, SyncPolicy::new(3));
    let status = pass(&bounded);

    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(
        status.backfill.expect("eight blocks are owed").pending,
        1,
        "the gap is a fact on the status, not a reason to answer nothing"
    );
}

/// A network that has not reached its tip is a different state, and stays
/// one: "this index cannot answer for the chain yet" must not collapse into
/// "one old range cannot be finished".
#[test]
fn a_network_behind_its_tip_is_still_catching_up() {
    let memory = memory::Repository::new();
    let source = source(10);
    caught_up(&memory, source.clone());
    let behind = memory_service(&memory, source.at(20), SyncPolicy::new(3));

    let status = pass(&behind);

    assert_eq!(status.phase, SyncPhase::CatchingUp);
    assert!(status.backfill.is_none(), "nothing is watched at all");
}
