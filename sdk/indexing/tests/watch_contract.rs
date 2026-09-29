//! What the index owns after `docs/design/030 §8 (ex-029)`: the list of what it observes,
//! the durable row behind it, and the hook that says what a commit changed.
//!
//! A wallet and a bare holding reach the same call, both are indexed, and both
//! come back after a restart from the registry alone — no caller re-states
//! anything. The listener is a list rather than a slot, it carries what the
//! commit actually produced, and it cannot stall the pass because it cannot
//! await.

use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use futures_executor::block_on;
use indexing::{
    Address, AssetId, BlockCommit, BlockHash, BlockHeight, BlockInterpreter, BlockParent,
    BlockPosition, BlockRef, BlockSource, BoxFuture, ChainName, ChainTip, Decimal, History, Holder,
    IndexError, IndexErrorKind, IndexedBlock, IndexedOutput, Indexing, InterpretedBlock, Listener,
    MovementId, Network, ObservationDraft, ObservationDraftStatus, OutputChanges, OutputId,
    OutputKey, Service, Start, Subject, SyncConfig, SyncPolicy, TransactionRef, ValueMovement,
    Watching, memory,
};

/// Every block this source serves, one per position, each crediting the
/// address it was told to credit.
#[derive(Clone)]
struct Source(u64);

#[derive(Clone, Debug)]
struct Block(BlockRef);

impl IndexedBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

fn network() -> Network {
    Network::new(ChainName::new("watched"), "testnet")
}

fn asset() -> AssetId {
    AssetId::native(network().chain)
}

fn address(value: &str) -> Address {
    Address::new(network(), value)
}

fn subject(value: &str) -> Subject {
    Subject {
        address: address(value),
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

impl ChainTip for Source {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, indexing::SourceError>> {
        Box::pin(async move { Ok(block(self.0)) })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, indexing::SourceError>> {
        Box::pin(async move { Ok(block(self.0)) })
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
        let blocks = (start.0..=end.0.min(self.0))
            .take(limit)
            .map(|height| Block(block(height)))
            .collect();
        Box::pin(async move { Ok(blocks) })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, indexing::SourceError>> {
        let found = (position.0 <= self.0).then(|| block(position.0));
        Box::pin(async move { Ok(found) })
    }
}

/// Credits every selected address in every block, and creates one output for
/// each, so a commit carries transactions *and* output changes. Block 2 also
/// spends an output nothing indexed, so it carries an unknown spend.
#[derive(Clone, Default)]
struct Interpreter;

impl BlockInterpreter for Interpreter {
    type Block = Block;

    fn settlement(&self) -> indexing::FeeSettlement {
        indexing::FeeSettlement::Charged
    }

    fn inspect(
        &self,
        source: &Self::Block,
        addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        let reference = source.block_ref();
        let mut transactions = Vec::new();
        let mut created = Vec::new();
        for (order, owner) in (0_u64..).zip(addresses) {
            let id = TransactionRef::new(
                network(),
                format!("tx-{}-{}", reference.height.0, owner.value),
            );
            transactions.push(ObservationDraft {
                transaction_id: id.clone(),
                transaction_order: order,
                status: ObservationDraftStatus::Included,
                movements: vec![ValueMovement::output(
                    MovementId("output-0".into()),
                    asset(),
                    Decimal::from(5_u64),
                    Some(owner.clone()),
                )],
                fee: None,
            });
            created.push(IndexedOutput {
                id: OutputId {
                    transaction: id,
                    index: 0,
                },
                address: owner.clone(),
                asset: asset(),
                amount: Decimal::from(5_u64),
                evidence: Vec::new(),
                created_at: reference.height,
                coinbase: false,
            });
        }
        let spent = match (reference.height.0, addresses.first()) {
            (2, Some(owner)) => vec![OutputKey {
                address: owner.clone(),
                output: OutputId {
                    transaction: TransactionRef::new(network(), "never-indexed"),
                    index: 0,
                },
            }],
            _ => Vec::new(),
        };
        Ok(InterpretedBlock {
            block: reference,
            transactions,
            outputs: OutputChanges {
                created,
                spent,
                tracked_spends: Vec::new(),
            },
            credits: Vec::new(),
        })
    }
}

fn service(
    memory: &memory::Repository,
    tip: u64,
) -> Arc<Service<Source, Interpreter, memory::Repository>> {
    Arc::new(Service::new(
        Source(tip),
        Interpreter,
        memory.clone(),
        SyncConfig::new(&network(), SyncPolicy::new(100)).expect("sync config"),
    ))
}

/// What a consumer reads: the history reader over this index, which is the
/// repository's own page joined to the blocks the index still holds.
fn indexed(
    index: &Arc<Service<Source, Interpreter, memory::Repository>>,
    owner: &Address,
) -> Vec<TransactionRef> {
    let subject = indexing::Subject {
        address: owner.clone(),
        asset: asset(),
    };
    let reader = index.history();
    block_on(reader.history(&subject, indexing::Page::first(50)))
        .expect("history")
        .transactions
        .into_iter()
        .map(|entry| entry.transaction_id)
        .collect()
}

/// A wallet and a bare holding reach the same call, both are indexed, and a
/// restart on the same store resumes with both — from the registry alone.
#[test]
fn a_watched_wallet_and_a_watched_address_are_indexed_and_survive_a_restart() {
    let memory = memory::Repository::new();
    let wallet = subject("wallet");
    let observed = subject("observed");
    let index = service(&memory, 3);

    block_on(index.watch("wallet-1", &wallet, Start::At(BlockPosition(0)))).expect("wallet");
    block_on(index.watch("observed-1", &observed, Start::At(BlockPosition(0)))).expect("address");
    block_on(index.sync()).expect("first pass");

    for subject in [&wallet, &observed] {
        assert!(
            !indexed(&index, subject.address()).is_empty(),
            "{} must be indexed",
            subject.address().value
        );
    }

    // A restart: a new service over the same store, told nothing by anyone.
    drop(index);
    let restarted = service(&memory, 5);
    assert_eq!(
        block_on(restarted.watched(&network()))
            .expect("restored watch list")
            .into_iter()
            .map(|watch| (watch.id, watch.address.value, watch.start))
            .collect::<Vec<_>>(),
        [
            (
                "observed-1".to_owned(),
                "observed".to_owned(),
                BlockPosition(0)
            ),
            ("wallet-1".to_owned(), "wallet".to_owned(), BlockPosition(0)),
        ],
        "the registry is the durable mirror of the watch list, read in id order"
    );

    block_on(restarted.sync()).expect("restarted pass");
    for subject in [&wallet, &observed] {
        assert_eq!(
            indexed(&restarted, subject.address()).len(),
            6,
            "{} keeps being indexed after the restart",
            subject.address().value
        );
    }
}

/// A watch is idempotent under the same id and start, and refuses every
/// collision rather than quietly replacing a row.
#[test]
fn re_watching_is_a_no_op_and_every_collision_is_a_conflict() {
    let memory = memory::Repository::new();
    let index = service(&memory, 1);
    let owner = subject("owner");
    let first = block_on(index.watch("id", &owner, Start::At(BlockPosition(2)))).expect("watch");

    assert_eq!(
        block_on(index.watch("id", &owner, Start::At(BlockPosition(2)))).expect("re-watch"),
        first,
        "re-stating the same watch changes nothing"
    );
    for (id, start) in [("id", BlockPosition(9)), ("other", BlockPosition(2))] {
        let error = block_on(index.watch(id, &owner, Start::At(start)))
            .expect_err("a known address is watched once");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
    }
    let error = block_on(index.watch("id", &subject("elsewhere"), Start::Fresh))
        .expect_err("a known id names one address");
    assert_eq!(error.kind, IndexErrorKind::Conflict);
    assert_eq!(block_on(index.watched(&network())).expect("rows"), [first]);
}

/// How far one pass said the network still owes, and how many blocks it read
/// below its checkpoint getting there.
fn owed(index: &dyn Indexing) -> Option<(BlockPosition, BlockPosition, usize)> {
    block_on(index.sync())
        .expect("pass")
        .backfill
        .map(|status| (status.resume, status.anchor, status.inspected))
}

/// Withdrawing a watch stops inspection and deletes nothing, the removal is
/// durable, and re-watching the address inherits what the store proved was read
/// while still owing what nobody read.
///
/// The questions `docs/design/030 §8 (ex-029)` section 5.2 refused an `unwatch` over,
/// answered one assertion each. Note what is *not* asserted: that the rows
/// become unreadable. They do not, and must not -- a read answers for an
/// `indexing::Holder` and consults no watch list, which is why an application
/// that lost the id is the thing that cannot reach them.
#[test]
fn unwatching_stops_inspection_keeps_every_row_and_can_be_undone() {
    let memory = memory::Repository::new();
    let index = service(&memory, 3);
    let owner = subject("owner");
    block_on(index.sync()).expect("a checkpoint to owe history below");
    block_on(index.watch("owner", &owner, Start::At(BlockPosition(0)))).expect("watch");
    assert_eq!(
        owed(&*index),
        None,
        "the pass settles the range it owed, so nothing is left"
    );
    let filled = indexed(&index, owner.address());
    assert_eq!(filled.len(), 4, "blocks 0..=3 were read for it");

    assert_eq!(
        block_on(index.unwatch("nobody")).expect("an unknown id is not an error"),
        None,
        "unwatching what nothing watches is idempotent, not a refusal"
    );
    let removed = block_on(index.unwatch("owner"))
        .expect("unwatch")
        .expect("the row it removed");
    assert_eq!(removed.id, "owner");
    assert!(
        block_on(index.watched(&network()))
            .expect("rows")
            .is_empty()
    );
    assert_eq!(
        indexed(&index, owner.address()),
        filled,
        "nothing indexed was deleted; the rows are simply unclaimed"
    );

    // Durable, and the pass that follows never inspects the address again: a
    // restart is told nothing by anyone, and blocks 4 and 5 are new.
    drop(index);
    let restarted = service(&memory, 5);
    assert!(
        block_on(restarted.watched(&network()))
            .expect("rows")
            .is_empty(),
        "the registration is gone, so a restart does not observe it again"
    );
    block_on(restarted.sync()).expect("pass");
    assert_eq!(
        indexed(&restarted, owner.address()),
        filled,
        "blocks 4 and 5 were never inspected for an address nothing watched"
    );

    // Watched again from the same birthday: the claim is gone with the watch, so
    // the range is read again rather than inherited — which is what stops blocks
    // 4 and 5, committed while nothing watched the address, from being settled
    // by a claim that was true only up to block 3.
    block_on(restarted.watch("owner", &owner, Start::At(BlockPosition(0)))).expect("re-watch");
    assert_eq!(
        owed(&*restarted),
        None,
        "the re-watch's whole range is read in the pass that follows it"
    );
    assert_eq!(
        indexed(&restarted, owner.address()).len(),
        6,
        "blocks 4 and 5 are there too, and re-reading 0..=3 duplicated nothing"
    );
}

/// Cancelling settles every range the network owes, leaves each watch as the one
/// it would have been created now, keeps every row already indexed, and can be
/// undone by asking for the range again.
#[test]
fn cancelling_a_backfill_settles_its_range_and_leaves_the_watch_at_the_checkpoint() {
    let memory = memory::Repository::new();
    let index = service(&memory, 3);
    let owner = subject("owner");
    block_on(index.sync()).expect("a checkpoint to owe history below");
    assert!(
        block_on(indexing::Backfilling::cancel(&*index, &network()))
            .expect("cancel")
            .is_empty(),
        "a network that owes nothing is cancelled without touching a watch"
    );
    block_on(index.watch("owner", &owner, Start::At(BlockPosition(0)))).expect("watch");

    let cancelled = block_on(indexing::Backfilling::cancel(&*index, &network())).expect("cancel");

    assert_eq!(
        cancelled
            .iter()
            .map(|watch| (watch.id.clone(), watch.start))
            .collect::<Vec<_>>(),
        [("owner".to_owned(), BlockPosition(4))],
        "the watch is left where a `Start::Fresh` one added now would begin"
    );
    assert_eq!(
        block_on(index.watched(&network())).expect("rows"),
        cancelled,
        "the same id and address, from the raised birthday"
    );
    assert_eq!(owed(&*index), None, "the range is no longer asked for");
    assert!(
        indexed(&index, owner.address()).is_empty(),
        "nothing below the cancelled range was read"
    );

    // And the range can be asked for again, at the price of reading it.
    block_on(index.unwatch("owner")).expect("unwatch");
    block_on(index.watch("owner", &owner, Start::At(BlockPosition(0)))).expect("re-watch");
    assert_eq!(owed(&*index), None);
    assert_eq!(
        indexed(&index, owner.address()).len(),
        4,
        "a cancelled range is resumable: the claim was dropped, never the ability"
    );
}

#[derive(Default)]
struct Seen(Mutex<Vec<BlockCommit>>);

impl Listener for Seen {
    fn committed(&self, commit: &BlockCommit) {
        self.0.lock().expect("commits").push(commit.clone());
    }
}

/// A listener is a list, not a slot, and each entry is told what the commit
/// actually produced: the block, its transactions, its output changes, and
/// the required spends the store could not match.
#[test]
fn every_listener_is_told_what_each_commit_changed() {
    let memory = memory::Repository::new();
    let index = service(&memory, 2);
    let owner = subject("owner");
    block_on(index.watch("owner", &owner, Start::At(BlockPosition(0)))).expect("watch");
    let first = Arc::new(Seen::default());
    let second = Arc::new(Seen::default());
    index.listen(first.clone());
    index.listen(second.clone());

    block_on(index.sync()).expect("pass");

    for seen in [&first, &second] {
        let commits = seen.0.lock().expect("commits");
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit.block.height.0)
                .collect::<Vec<_>>(),
            [0, 1, 2],
            "one call per committed block, oldest first, for every listener"
        );
        assert!(commits.iter().all(|commit| commit.network == network()));
        assert_eq!(commits[1].transactions.len(), 1, "what it indexed");
        assert_eq!(commits[1].outputs.created.len(), 1, "what became spendable");
        assert!(commits[1].unknown_spends.is_empty());
        assert_eq!(
            commits[2]
                .unknown_spends
                .iter()
                .map(|key| key.output.transaction.id.as_str())
                .collect::<Vec<_>>(),
            ["never-indexed"],
            "a required spend nothing indexed is a completeness fact the \
             listener is told about, not a failure"
        );
    }

    // Nothing is announced twice: a replayed tip is `AlreadyApplied`.
    block_on(index.sync()).expect("second pass");
    assert_eq!(first.0.lock().expect("commits").len(), 3);
}

/// A listener that burns time delays only the chain it is registered on, and
/// it cannot stall the pass by awaiting, because it cannot await at all: the
/// commit loop calls it synchronously and moves on.
#[test]
fn a_slow_listener_delays_its_own_chain_and_cannot_await() {
    /// Sleeps in `committed`, which is the worst a listener can do.
    struct Slow(Duration, Mutex<usize>);

    impl Listener for Slow {
        fn committed(&self, _commit: &BlockCommit) {
            std::thread::sleep(self.0);
            *self.1.lock().expect("calls") += 1;
        }
    }

    let memory = memory::Repository::new();
    let index = service(&memory, 2);
    block_on(index.watch("owner", &subject("owner"), Start::At(BlockPosition(0)))).expect("watch");
    let slow = Arc::new(Slow(Duration::from_millis(20), Mutex::new(0)));
    index.listen(slow.clone());

    let started = Instant::now();
    block_on(index.sync()).expect("pass");
    let elapsed = started.elapsed();

    assert_eq!(
        *slow.1.lock().expect("calls"),
        3,
        "every block was announced"
    );
    assert!(
        elapsed >= Duration::from_millis(60),
        "the pass waited for its own listener: {elapsed:?}"
    );
    // And it still committed everything: slowness costs latency on this chain,
    // never correctness, and never another chain's pass — units run on their
    // own tasks.
    assert_eq!(
        block_on(indexing::Checkpoint::checkpoint(&*index, &network()))
            .expect("checkpoint")
            .map(|block| block.height.0),
        Some(2)
    );
}

/// A listener's callback is arbitrary consumer code, and the plainest thing it
/// can do is register another listener. The dispatch works from a snapshot of
/// the list rather than holding it across the call, so that registration lands
/// instead of deadlocking the pass with no diagnostic at all.
///
/// The listener registered from inside a dispatch joins at the *next* block:
/// delivery stays at most once and is never retracted, and a listener is never
/// told about a block whose announcement had already begun.
#[test]
fn a_listener_that_registers_another_listener_does_not_deadlock() {
    /// Registers `late` from inside `committed`, once, which is the re-entrant
    /// call a list held across the dispatch cannot take.
    struct Reentrant {
        index: Weak<Service<Source, Interpreter, memory::Repository>>,
        late: Arc<Seen>,
        registered: AtomicBool,
    }

    impl Listener for Reentrant {
        fn committed(&self, _commit: &BlockCommit) {
            if self.registered.swap(true, Ordering::SeqCst) {
                return;
            }
            let index = self.index.upgrade().expect("the index outlives its pass");
            index.listen(self.late.clone());
        }
    }

    let memory = memory::Repository::new();
    let index = service(&memory, 2);
    block_on(index.watch("owner", &subject("owner"), Start::At(BlockPosition(0)))).expect("watch");
    let late = Arc::new(Seen::default());
    index.listen(Arc::new(Reentrant {
        index: Arc::downgrade(&index),
        late: late.clone(),
        registered: AtomicBool::new(false),
    }));

    // The pass runs on a thread of its own so a deadlock fails this test on a
    // bounded wait instead of hanging the suite forever.
    let (finished, pass) = mpsc::channel();
    let syncing = index.clone();
    let passing = std::thread::spawn(move || {
        finished.send(block_on(syncing.sync()).is_ok()).ok();
    });
    assert!(
        pass.recv_timeout(Duration::from_secs(5))
            .expect("the pass announced its commits without deadlocking on the listener list"),
        "the pass itself succeeded"
    );
    passing.join().expect("the pass thread");

    assert_eq!(
        late.0
            .lock()
            .expect("commits")
            .iter()
            .map(|commit| commit.block.height.0)
            .collect::<Vec<_>>(),
        [1, 2],
        "the re-entrant registration landed, and joins at the block after the \
         announcement it was made from"
    );
    assert_eq!(
        block_on(indexing::Checkpoint::checkpoint(&*index, &network()))
            .expect("checkpoint")
            .map(|block| block.height.0),
        Some(2)
    );
}
