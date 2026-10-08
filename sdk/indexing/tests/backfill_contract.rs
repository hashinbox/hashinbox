//! Late observations backfill their own history without delaying forward sync.

use std::sync::Arc;

mod rows;

use base::Decimal;
use indexing::{
    Address, AssetId, Birthday, BlockHeight, BlockPosition, Budget, ChainName, Config,
    IndexErrorKind, Indexer, MovementId, Network, Outcome, Phase, Status, Subject, Transaction,
    TransactionRef, ValueMovement, Watching, sqlite,
    testing::{Chain, Fault, block},
};
use rows::Rows;
use tempfile::TempDir;

/// The SQLite store runs on `sqlx`, which needs a Tokio runtime with its
/// timer rather than a bare executor.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("test runtime")
        })
        .block_on(future)
}

/// A fresh store on a temporary file, a reader of its rows, and the directory
/// that holds it.
fn store() -> (sqlite::Repository, Rows, TempDir) {
    let directory = TempDir::new().expect("temporary directory");
    let database = block_on(sqlite::Database::open(directory.path().join("index.db")))
        .expect("temporary sqlite database");
    let rows = Rows::new(database.pool().clone());
    (sqlite::Repository::new(database), rows, directory)
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

/// Every address a scenario here watches.
const WATCHED: [&str; 6] = ["late", "fresh", "early", "x", "y", "z"];

type Index<R> = Indexer<Chain, Chain, R>;

fn service<R: Clone>(repository: R, source: Chain, policy: Budget) -> Arc<Index<R>> {
    Arc::new(Indexer::new(
        source.clone(),
        source,
        repository,
        Config::new(&network(), policy).expect("sync config"),
    ))
}

fn indexer_over(
    repository: &sqlite::Repository,
    source: Chain,
    policy: Budget,
) -> Arc<Index<sqlite::Repository>> {
    service(repository.clone(), source, policy)
}

/// The chain of [`source`] through 10, failing with `fault`: a node that has
/// pruned what it was asked for, or that leaves one produced block out of
/// every range it serves while answering for every other one — the shape a
/// claim must never be built over, because the block it omits is one nothing
/// will ever inspect again.
fn failing(fault: Fault) -> Chain {
    let chain = source(10);
    chain.fail(fault);
    chain
}

/// One pass, returning the status of the only network.
fn pass<R>(index: &Arc<Index<R>>) -> Outcome
where
    Index<R>: indexing::Indexing,
{
    block_on(indexing::Indexing::sync(&**index)).expect("pass")
}

/// What the index wrote: the stored history of `address`, which is where a
/// pass's commits are read back. No query goes through the index.
fn indexed(rows: &Rows, address: &str) -> Vec<String> {
    block_on(rows.history(&Address::new(network(), address), &asset(), 100))
}

/// Ten blocks indexed with nothing watched, so the store holds a checkpoint
/// and no history at all.
fn caught_up(repository: &sqlite::Repository, source: Chain) -> Arc<Index<sqlite::Repository>> {
    let index = indexer_over(repository, source, Budget::new(100));
    assert_eq!(pass(&index).phase, Phase::Ready);
    index
}

/// A dense chain through `tip` on which every block credits each of
/// [`WATCHED`] 10 from the treasury, one transaction per address. A block
/// inspected with an empty selection produces no row at all, which is what
/// leaves the gap this test closes.
fn source(tip: u64) -> Chain {
    let chain = Chain::dense(network(), tip);
    for height in 0..=30 {
        for (order, address) in (0_u64..).zip(WATCHED) {
            chain.include(&block(height), credit(height, order, address));
        }
    }
    chain
}

/// The treasury's transfer of 10 to `address` in block `height`, at `order`
/// in that block: every address gets its own transaction in the same block,
/// so the ordinal is the address's place in the block's whole list.
fn credit(height: u64, order: u64, address: &str) -> Transaction {
    let address = Address::new(network(), address);
    Transaction {
        id: TransactionRef::new(network(), format!("{}-{height}", address.value)),
        movements: vec![ValueMovement::transfer(
            MovementId("0".into()),
            asset(),
            Decimal::from(10_u64),
            Address::new(network(), "treasury"),
            address,
        )],
        fee: None,
        order,
        status: Status::Included {
            block: block(height),
        },
    }
}

/// The property: a watch below the checkpoint is honoured, not discarded.
#[test]
fn an_address_watched_from_its_own_past_ends_up_with_that_history() {
    let (repository, rows, _directory) = store();
    let index = caught_up(&repository, source(10));
    let late = subject("late");

    block_on(index.watch("late-1", &late, Birthday::At(BlockPosition(2)))).expect("watch");
    assert!(indexed(&rows, "late").is_empty(), "nothing is indexed yet");

    let status = pass(&index);

    assert_eq!(
        indexed(&rows, "late").len(),
        9,
        "blocks 2 through 10 are indexed for an address watched at 2"
    );
    assert_eq!(status.checkpoint, Some(block(10)), "nothing moved forward");
    assert!(status.backfill.is_none(), "one pass closed the whole gap");
    assert_eq!(status.phase, Phase::Ready);
}

/// An address watched from *now* owes nothing and is never swept.
#[test]
fn a_fresh_watch_owes_nothing_and_reads_no_historical_block() {
    let (repository, _rows, _directory) = store();
    let source = source(10);
    let index = caught_up(&repository, source.clone());
    let read = source.fetched().len();

    block_on(index.watch("fresh-1", &subject("fresh"), Birthday::Fresh)).expect("watch");
    let status = pass(&index);

    assert!(status.backfill.is_none(), "a fresh watch owes nothing");
    assert_eq!(
        source.fetched().len(),
        read,
        "and so costs no block fetch at all"
    );
}

/// A pass reads no more than its batch size, and what it leaves it reports.
#[test]
fn one_pass_reads_no_more_blocks_than_its_batch_size() {
    let (repository, rows, _directory) = store();
    let source = source(10);
    let index = caught_up(&repository, source.clone());
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
        .expect("watch");

    let bounded = indexer_over(&repository, source.clone(), Budget::new(3));
    let read = source.fetched().len();
    let status = pass(&bounded);

    assert_eq!(
        source.fetched().len() - read,
        3,
        "the backfill reads the batch size and stops"
    );
    let backfill = status.backfill.expect("eight blocks are still owed");
    assert_eq!(backfill.pending, 1);
    assert_eq!(backfill.inspected, 3);
    assert_eq!(backfill.resume, BlockPosition(3));
    assert_eq!(backfill.anchor, BlockPosition(10));
    assert_eq!(indexed(&rows, "late").len(), 3);
}

/// A backfill interrupted by a restart resumes where it stopped and reaches
/// the same end state as one that ran straight through.
#[test]
fn an_interrupted_backfill_reaches_the_same_end_state() {
    let uninterrupted = {
        let (repository, rows, _directory) = store();
        let source = source(10);
        let index = caught_up(&repository, source.clone());
        block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
            .expect("watch");
        let straight = indexer_over(&repository, source, Budget::new(100));
        assert!(pass(&straight).backfill.is_none());
        indexed(&rows, "late")
    };

    let (repository, rows, _directory) = store();
    let source = source(10);
    let index = caught_up(&repository, source.clone());
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
        .expect("watch");

    // Three blocks, then the process dies: a new service over the same store,
    // told nothing by anyone.
    let first = indexer_over(&repository, source.clone(), Budget::new(3));
    assert_eq!(pass(&first).backfill.expect("owed").inspected, 3);
    drop(first);

    let resumed = indexer_over(&repository, source.clone(), Budget::new(100));
    let read = source.fetched().len();
    assert!(
        pass(&resumed).backfill.is_none(),
        "the rest of the range closes in one pass"
    );
    assert_eq!(
        source.fetched().len() - read,
        8,
        "the resumed pass reads blocks 3 to 10 and never rereads 0 to 2"
    );

    let mut resumed_history = indexed(&rows, "late");
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
    let (repository, rows, _directory) = store();
    let source = source(10);
    let index = caught_up(&repository, source.clone());
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(2))))
        .expect("watch");
    assert!(pass(&index).backfill.is_none());
    drop(index);

    let restarted = indexer_over(&repository, source.clone(), Budget::new(100));
    let read = source.fetched().len();
    let status = pass(&restarted);

    assert!(status.backfill.is_none(), "nothing is owed after a restart");
    assert_eq!(
        source.fetched().len(),
        read,
        "and no block is fetched to find that out"
    );
    assert_eq!(indexed(&rows, "late").len(), 9);
}

/// A second observation of a watched address, born below what is already
/// recorded for the first, survives a restart taken before any pass.
///
/// **The defect this pins is now structurally impossible, and that is the point
/// of keeping the test.** Progress used to be one row per *address*: registering
/// a second observation from further back lowered that shared row in repository, and
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
    let (repository, rows, _directory) = store();
    let source = source(10);
    let index = caught_up(&repository, source.clone());

    // The first observation settles its own range, durably.
    block_on(index.watch(
        "late-native",
        &subject("late"),
        Birthday::At(BlockPosition(7)),
    ))
    .expect("the first observation");
    assert!(pass(&index).backfill.is_none(), "7..10 is settled");
    assert_eq!(indexed(&rows, "late").len(), 4, "blocks 7 to 10");

    // A second asset at the same address, from further back, and then the
    // process dies before any pass can record the wider claim.
    let token = Subject {
        address: Address::new(network(), "late"),
        asset: AssetId {
            chain: network().chain,
            asset: "token".into(),
        },
    };
    block_on(index.watch("late-token", &token, Birthday::At(BlockPosition(2))))
        .expect("the second observation");
    drop(index);

    let restarted = indexer_over(&repository, source.clone(), Budget::new(100));
    let read = source.fetched().len();

    assert!(
        pass(&restarted).backfill.is_none(),
        "one pass closes the widened range"
    );
    assert!(
        source.fetched().len() > read,
        "and it read blocks to do it, rather than settling a range it never saw"
    );
    assert_eq!(
        indexed(&rows, "late").len(),
        9,
        "blocks 2 to 10, not just the 7 to 10 the first observation claimed"
    );
}

/// Independent observations at one address keep their own contiguous progress.
#[test]
fn one_pass_serves_three_observations_from_their_own_positions() {
    let (repository, rows, _directory) = store();
    let source = source(30);
    // A line at 30 with nothing watched, so every observation below is behind it.
    let index = caught_up(&repository, source.clone());

    // Z, born at 10, reads all the way to the line and joins the front.
    block_on(index.watch("z", &subject("z"), Birthday::At(BlockPosition(10)))).expect("z");
    assert!(pass(&index).backfill.is_none(), "z closes its whole range");
    let z_history = indexed(&rows, "z").len();
    assert_eq!(z_history, 21, "blocks 10 through 30");

    // X, born at 20, reads exactly six blocks and stops: one bounded pass.
    block_on(index.watch("x", &subject("x"), Birthday::At(BlockPosition(20)))).expect("x");
    let bounded = service(repository.clone(), source.clone(), Budget::new(6));
    let stopped = pass(&bounded).backfill.expect("x still owes 26 to 30");
    assert_eq!(stopped.inspected, 6, "blocks 20 through 25");
    assert_eq!(stopped.resume, BlockPosition(26), "x resumes at 26");
    assert_eq!(indexed(&rows, "x").len(), 6);
    drop(bounded);

    // Y, born at 22, has had nothing inspected for it at all.
    block_on(index.watch("y", &subject("y"), Birthday::At(BlockPosition(22)))).expect("y");
    drop(index);

    let one = service(repository.clone(), source.clone(), Budget::new(100));
    let read = source.fetched().len();
    let inspected = source.inspections().len();
    assert!(
        pass(&one).backfill.is_none(),
        "one pass closes what x and y owe between them"
    );

    assert_eq!(
        source.fetched().len() - read,
        9,
        "blocks 22 through 30, fetched once for the two observations that need \
         them rather than once each"
    );
    assert_eq!(
        source.inspections()[inspected..],
        (22..=30)
            .map(|height| (
                BlockHeight(height),
                if height <= 25 {
                    vec![subject("y").address]
                } else {
                    vec![subject("x").address, subject("y").address]
                }
            ))
            .collect::<Vec<_>>(),
        "22 to 25 inspected for y alone, 26 to 30 for x and y, and nothing for z"
    );
    assert_eq!(indexed(&rows, "y").len(), 9, "y: blocks 22 through 30");
    assert_eq!(indexed(&rows, "x").len(), 11, "x: blocks 20 through 30");
    assert_eq!(
        indexed(&rows, "z").len(),
        z_history,
        "and z is untouched: adding y reset neither of the others"
    );
}

/// A start further below the checkpoint than the policy allows is refused by
/// the call that asked for it, naming the earliest start it would accept.
#[test]
fn a_start_below_the_policy_limit_is_refused_synchronously() {
    let (repository, _rows, _directory) = store();
    let index = indexer_over(&repository, source(10), Budget::new(100).with_backfill(4));
    assert_eq!(pass(&index).phase, Phase::Ready);

    let error = block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(2))))
        .expect_err("a start nine blocks below the checkpoint");

    assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
    assert!(!error.retryable, "the caller must choose another start");
    assert!(
        error.message.contains("earliest acceptable start is 6"),
        "the refusal names a start that would work: {}",
        error.message
    );
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(6))))
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
    let (repository, rows, _directory) = store();
    let source = source(10);
    let index = caught_up(&repository, source.clone());
    block_on(index.watch("fresh-1", &subject("fresh"), Birthday::Fresh)).expect("watch");
    assert!(
        pass(&index).backfill.is_none(),
        "a fresh watch owes nothing"
    );
    drop(index);

    // The chain moves on and the process restarts. The forward pass indexes
    // the watched address for every block it passes, which is the whole of
    // what that watch asked for, so those ten blocks are the only ones any
    // phase of this pass has a reason to read.
    source.extend_to(20);
    let moved = source;
    let ahead = indexer_over(&repository, moved.clone(), Budget::new(100));
    let read = moved.fetched().len();
    assert_eq!(pass(&ahead).phase, Phase::Ready);
    assert_eq!(
        moved.fetched().len() - read,
        10,
        "blocks 11 through 20, read once by the forward phase and not swept again"
    );
    assert_eq!(indexed(&rows, "fresh").len(), 10, "blocks 11 through 20");
    drop(ahead);

    let restarted = indexer_over(&repository, moved.clone(), Budget::new(100));
    let read = moved.fetched().len();
    let status = pass(&restarted);

    assert!(status.backfill.is_none(), "nothing is owed after a restart");
    assert_eq!(
        moved.fetched().len(),
        read,
        "and no block is re-read to find that out"
    );
    assert_eq!(indexed(&rows, "fresh").len(), 10);
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
    let (repository, rows, _directory) = store();
    let index = caught_up(&repository, source(10));
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
        .expect("watch");
    drop(index);

    // The same node, having lost one block in the middle of the range.
    let gapped = indexer_over(
        &repository,
        failing(Fault::Omitted(BlockPosition(5))),
        Budget::new(100),
    );
    let status = pass(&gapped);

    let backfill = status.backfill.expect("the range is still owed");
    let failure = backfill.failure.expect("and the pass says why");
    assert_eq!(failure.kind, IndexErrorKind::Disconnected);
    assert!(failure.retryable, "a node may serve the block next time");
    assert_eq!(
        backfill.resume,
        BlockPosition(0),
        "nothing was settled, so the whole range is still owed"
    );
    assert!(
        indexed(&rows, "late").is_empty(),
        "and no row of the partial reading was written"
    );
    assert_eq!(
        status.phase,
        Phase::Ready,
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
    let (repository, rows, _directory) = store();
    let index = caught_up(&repository, source(10));
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
        .expect("watch");
    let bounded = indexer_over(&repository, source(10), Budget::new(3));
    assert_eq!(
        pass(&bounded).backfill.expect("owed").resume,
        BlockPosition(3)
    );
    drop(bounded);

    let gapped = indexer_over(
        &repository,
        failing(Fault::Omitted(BlockPosition(3))),
        Budget::new(100),
    );
    let status = pass(&gapped);

    let backfill = status
        .backfill
        .expect("the rest of the range is still owed");
    assert_eq!(
        backfill.failure.expect("and the pass says why").kind,
        IndexErrorKind::Disconnected
    );
    assert_eq!(
        backfill.resume,
        BlockPosition(3),
        "the observation still resumes at the block it never read"
    );
    assert_eq!(
        indexed(&rows, "late").len(),
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
    let (repository, _rows, _directory) = store();
    let pruned = failing(Fault::Pruned {
        below: BlockPosition(5),
        retryable: true,
    });
    let index = caught_up(&repository, pruned.clone());
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
        .expect("watch");

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

/// The availability floor : a historical range the
/// node will never serve must not make the network unanswerable.
///
/// A pruned range is a property of one watch's request, not of the chain. The
/// checkpoint is at the tip, every block above the birthday is indexed, and
/// every read that the missing range weakens already says so through
/// how far behind its rows are. Reporting `CatchingUp` for a range that can never close
/// held `/health/ready` at 503 for the life of the process — and, for a
/// wallet configured before the supervisor starts, stopped the HTTP listener
/// binding at all. So the phase follows forward progress, and the owed range
/// is reported beside it rather than by withholding the whole API.
#[test]
fn a_backfill_the_source_cannot_serve_still_answers_for_the_tip() {
    let (repository, _rows, _directory) = store();
    let pruned = failing(Fault::Pruned {
        below: BlockPosition(5),
        retryable: true,
    });
    let index = caught_up(&repository, pruned);
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
        .expect("watch");

    let status = pass(&index);

    assert_eq!(
        status.phase,
        Phase::Ready,
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
    let (repository, _rows, _directory) = store();
    let source = source(10);
    let index = caught_up(&repository, source.clone());
    block_on(index.watch("late-1", &subject("late"), Birthday::At(BlockPosition(0))))
        .expect("watch");

    let bounded = indexer_over(&repository, source, Budget::new(3));
    let status = pass(&bounded);

    assert_eq!(status.phase, Phase::Ready);
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
    let (repository, _rows, _directory) = store();
    let source = source(10);
    caught_up(&repository, source.clone());
    source.extend_to(20);
    let behind = indexer_over(&repository, source, Budget::new(3));

    let status = pass(&behind);

    assert_eq!(status.phase, Phase::CatchingUp);
    assert!(status.backfill.is_none(), "nothing is watched at all");
}
