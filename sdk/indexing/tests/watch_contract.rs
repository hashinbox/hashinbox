//! Observation ownership: durable watches, their hot mirror and commit listeners.

use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use indexing::{
    Address, AssetId, Birthday, BlockCommit, BlockHeight, BlockParent, BlockPosition, BlockRef,
    Budget, ChainName, Config, Decimal, Holder, IndexErrorKind, IndexedOutput, Indexer, Indexing,
    Listener, MovementId, Network, OutputChanges, OutputId, OutputKey, Repository, Status, Subject,
    Transaction, TransactionRef, ValueMovement, Watching, memory, sqlite,
    testing::{Chain, Fault, Request, block},
};

mod rows;

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
/// that holds it: for the scenarios that read back what a pass stored.
fn store() -> (sqlite::Repository, Rows, TempDir) {
    let directory = TempDir::new().expect("temporary directory");
    let database = block_on(sqlite::Database::open(directory.path().join("index.db")))
        .expect("temporary sqlite database");
    let rows = Rows::new(database.pool().clone());
    (sqlite::Repository::new(database), rows, directory)
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

/// Every address a scenario here watches.
const WATCHED: [&str; 5] = ["owner", "wallet", "observed", "new", "later"];

/// Pays each of [`WATCHED`] 5 in every block a scenario here reads, as one
/// transaction and one output, so a commit carries transactions *and* output
/// changes. Block 2 also spends, for `owner`, an output nothing indexed, so it
/// carries an unknown spend.
fn script(chain: &Chain) {
    for reference in (0..=5).map(block).chain([pruned(100), pruned(103)]) {
        for (order, owner) in (0_u64..).zip(WATCHED) {
            pay(chain, &reference, order, &address(owner));
        }
    }
    let unknown = OutputKey {
        address: address("owner"),
        output: OutputId {
            transaction: TransactionRef::new(network(), "never-indexed"),
            index: 0,
        },
    };
    chain.change_outputs(
        &block(2),
        OutputChanges {
            spent: vec![unknown],
            ..OutputChanges::default()
        },
    );
}

/// One transaction of block `reference` paying `owner` 5 at `order`, and the
/// output it creates.
fn pay(chain: &Chain, reference: &BlockRef, order: u64, owner: &Address) {
    let height = reference.height;
    let id = TransactionRef::new(network(), format!("tx-{}-{}", height.0, owner.value));
    let movement = ValueMovement::output(
        MovementId("output-0".into()),
        asset(),
        Decimal::from(5_u64),
        Some(owner.clone()),
    );
    chain.include(
        reference,
        Transaction {
            id: id.clone(),
            movements: vec![movement],
            fee: None,
            order,
            status: Status::Included {
                block: reference.clone(),
            },
        },
    );
    let output = IndexedOutput {
        id: OutputId {
            transaction: id,
            index: 0,
        },
        address: owner.clone(),
        asset: asset(),
        amount: Decimal::from(5_u64),
        evidence: Vec::new(),
        created_at: height,
        coinbase: false,
    };
    chain.change_outputs(
        reference,
        OutputChanges {
            created: vec![output],
            ..OutputChanges::default()
        },
    );
}

/// The dense chain through `tip`, scripted.
fn chain(tip: u64) -> Chain {
    let chain = Chain::dense(network(), tip);
    script(&chain);
    chain
}

fn service<R: Repository + Clone>(memory: &R, tip: u64) -> Arc<Indexer<Chain, Chain, R>> {
    let chain = chain(tip);
    Arc::new(Indexer::new(
        chain.clone(),
        chain,
        memory.clone(),
        Config::new(&network(), Budget::new(100)).expect("sync config"),
    ))
}

/// What the index wrote: the stored history of `owner`, which is where a
/// pass's commits are read back. No query goes through the index.
fn indexed(rows: &Rows, owner: &Address) -> Vec<TransactionRef> {
    block_on(rows.history(owner, &asset(), 50))
        .into_iter()
        .map(|id| TransactionRef::new(network(), id))
        .collect()
}

/// A wallet and a bare holding reach the same call, both are indexed, and a
/// restart on the same store resumes with both — from the registry alone.
#[test]
fn a_watched_wallet_and_a_watched_address_are_indexed_and_survive_a_restart() {
    let (memory, rows, _directory) = store();
    let wallet = subject("wallet");
    let observed = subject("observed");
    let index = service(&memory, 3);

    block_on(index.watch("wallet-1", &wallet, Birthday::At(BlockPosition(0)))).expect("wallet");
    block_on(index.watch("observed-1", &observed, Birthday::At(BlockPosition(0))))
        .expect("address");
    block_on(index.sync()).expect("first pass");

    for subject in [&wallet, &observed] {
        assert!(
            !indexed(&rows, subject.address()).is_empty(),
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
            indexed(&rows, subject.address()).len(),
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
    let first = block_on(index.watch("id", &owner, Birthday::At(BlockPosition(2)))).expect("watch");

    assert_eq!(
        block_on(index.watch("id", &owner, Birthday::At(BlockPosition(2)))).expect("re-watch"),
        first,
        "re-stating the same watch changes nothing"
    );
    for (id, start) in [("id", BlockPosition(9)), ("other", BlockPosition(2))] {
        let error = block_on(index.watch(id, &owner, Birthday::At(start)))
            .expect_err("a known address is watched once");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
    }
    let error = block_on(index.watch("id", &subject("elsewhere"), Birthday::Fresh))
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

/// Unwatch removes selection and progress without deleting retained history.
#[test]
fn unwatching_stops_inspection_keeps_every_row_and_can_be_undone() {
    let (memory, rows, _directory) = store();
    let index = service(&memory, 3);
    let owner = subject("owner");
    block_on(index.sync()).expect("a checkpoint to owe history below");
    block_on(index.watch("owner", &owner, Birthday::At(BlockPosition(0)))).expect("watch");
    assert_eq!(
        owed(&*index),
        None,
        "the pass settles the range it owed, so nothing is left"
    );
    let filled = indexed(&rows, owner.address());
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
        indexed(&rows, owner.address()),
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
        indexed(&rows, owner.address()),
        filled,
        "blocks 4 and 5 were never inspected for an address nothing watched"
    );

    // Watched again from the same birthday: the claim is gone with the watch, so
    // the range is read again rather than inherited — which is what stops blocks
    // 4 and 5, committed while nothing watched the address, from being settled
    // by a claim that was true only up to block 3.
    block_on(restarted.watch("owner", &owner, Birthday::At(BlockPosition(0)))).expect("re-watch");
    assert_eq!(
        owed(&*restarted),
        None,
        "the re-watch's whole range is read in the pass that follows it"
    );
    assert_eq!(
        indexed(&rows, owner.address()).len(),
        6,
        "blocks 4 and 5 are there too, and re-reading 0..=3 duplicated nothing"
    );
}

#[derive(Default)]
struct Seen(Mutex<Vec<BlockCommit>>);

impl Listener for Seen {
    fn committed(&self, commit: &BlockCommit) {
        self.0.lock().expect("commits").push(commit.clone());
    }
}

/// A listener is a list, not a slot, and each entry is told which block of
/// which network committed.
#[test]
fn every_listener_is_told_each_committed_block() {
    let memory = memory::Repository::new();
    let index = service(&memory, 2);
    let owner = subject("owner");
    block_on(index.watch("owner", &owner, Birthday::At(BlockPosition(0)))).expect("watch");
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
    block_on(index.watch("owner", &subject("owner"), Birthday::At(BlockPosition(0))))
        .expect("watch");
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
        index: Weak<Indexer<Chain, Chain, memory::Repository>>,
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
    block_on(index.watch("owner", &subject("owner"), Birthday::At(BlockPosition(0))))
        .expect("watch");
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

/// Slot `position` of a node that holds only recent sparse slots: 100 at
/// height 10 above a pruned 97, and 103 at height 11 above 100.
fn pruned(position: u64) -> BlockRef {
    let parent = if position == 100 { 97 } else { 100 };
    BlockRef {
        height: BlockHeight(if position == 100 { 10 } else { 11 }),
        parent: Some(BlockParent {
            position: BlockPosition(parent),
            hash: block(parent).hash,
        }),
        ..block(position)
    }
}

/// A node at slot 100 on which asking for the genesis range is terminal.
fn pruned_chain() -> Chain {
    let chain = Chain::new(network(), [pruned(100)]);
    chain.fail(Fault::Pruned {
        below: BlockPosition(100),
        retryable: false,
    });
    script(&chain);
    chain
}

#[test]
fn fresh_before_first_sync_uses_the_verified_tip_without_requiring_pruned_history() {
    use indexing::Checkpoint;
    let (memory, rows, _directory) = store();
    let source = pruned_chain();
    let index = Indexer::new(
        source.clone(),
        source.clone(),
        memory.clone(),
        Config::new(&network(), Budget::new(100)).expect("config"),
    );
    let owner = subject("new");
    let watched = block_on(index.watch("new", &owner, Birthday::Fresh)).expect("watch");
    assert_eq!(watched.start, BlockPosition(101));
    assert_eq!(
        block_on(index.checkpoint(&network())).expect("checkpoint"),
        None,
        "the source tip must not masquerade as a persisted CAS checkpoint"
    );
    let offline = source.requests();
    assert_eq!(
        block_on(index.watch("new", &owner, Birthday::Fresh)).expect("repeat"),
        watched
    );
    assert_eq!(source.requests(), offline, "unexpected RPC");
    block_on(index.sync()).expect("anchor without pruned history");
    source.replace([pruned(100), pruned(103)]);
    block_on(index.sync()).expect("sparse successor");
    assert_eq!(indexed(&rows, &owner.address).len(), 1);
    let offline = source.requests();
    let later = block_on(index.watch("later", &subject("later"), Birthday::Fresh))
        .expect("persisted checkpoint needs no RPC");
    assert_eq!(later.start, BlockPosition(104));
    assert_eq!(source.requests(), offline, "unexpected RPC");
}

#[test]
fn a_commit_during_fresh_tip_lookup_overrides_the_fallback_birthday() {
    let pause = Arc::new((
        AtomicBool::new(true),
        std::sync::Barrier::new(2),
        std::sync::Barrier::new(2),
    ));
    let source = pruned_chain();
    source.on_request({
        let pause = pause.clone();
        move |request| {
            if matches!(request, Request::Tip | Request::ConfirmedTip)
                && pause.0.swap(false, Ordering::SeqCst)
            {
                pause.1.wait();
                pause.2.wait();
            }
        }
    });
    let index = Arc::new(Indexer::new(
        source.clone(),
        source.clone(),
        memory::Repository::new(),
        Config::new(&network(), Budget::new(100)).expect("config"),
    ));
    let registering = index.clone();
    let thread = std::thread::spawn(move || {
        block_on(registering.watch("new", &subject("new"), Birthday::Fresh))
    });
    pause.1.wait();
    source.replace([pruned(100), pruned(103)]);
    block_on(index.sync()).expect("commit while the RPC is in flight");
    pause.2.wait();
    let watch = thread.join().expect("thread").expect("watch");
    assert_eq!(
        watch.start,
        BlockPosition(104),
        "the gate's committed line wins"
    );
}

/// The create flow over a registry: a wallet filed unobserved is not scanned
/// until the index watches its id, which observes the archived row rather than
/// refusing it; unwatching archives it again, and re-filing the address under
/// another id restores the original wallet, watched from its own birthday.
#[test]
fn a_filed_wallet_is_observed_by_watching_it_and_restored_at_its_birthday() {
    use indexing::{Registration, WalletRegistry as _};

    let memory = memory::Repository::new();
    let index = service(&memory, 3);
    block_on(index.sync()).expect("a checkpoint");
    let filing = |id: &str| Registration {
        id: id.to_owned(),
        address: address("new"),
        asset: asset(),
        label: Some("fresh".to_owned()),
        secret: Some(key()),
    };
    let filed = block_on(memory.register(filing("wallet"))).expect("filed");
    assert!(filed.archived_at().is_some());
    assert!(
        block_on(index.watched(&network()))
            .expect("rows")
            .is_empty(),
        "a filed wallet is not scanned"
    );

    let watched = block_on(index.watch("wallet", &filed, Birthday::Fresh)).expect("observed");
    assert_eq!(
        watched.start,
        BlockPosition(4),
        "fresh: after the checkpoint"
    );
    let stored = block_on(memory.wallet("wallet"))
        .expect("a read")
        .expect("kept");
    assert_eq!(stored.watch(), Some(watched.clone()), "the row is observed");
    block_on(index.sync()).expect("a pass");

    block_on(index.unwatch("wallet")).expect("archived");
    let restored = block_on(memory.register(filing("another"))).expect("restored");
    assert_eq!(restored.id(), "wallet", "the original wallet comes back");
    let birthday = restored.birthday().expect("its birthday is kept");
    let error = block_on(index.watch("another", &restored, Birthday::At(birthday)))
        .expect_err("the pair is reserved for its own id");
    assert_eq!(error.kind, IndexErrorKind::Conflict);
    let again =
        block_on(index.watch(restored.id(), &restored, Birthday::At(birthday))).expect("restored");
    assert_eq!(again, watched, "same id, holding and birthday");
    assert_eq!(
        block_on(index.watched(&network())).expect("rows"),
        [watched]
    );
}

/// A key no test reads back.
fn key() -> crypto::SecretBytes {
    crypto::SecretBytes::new([3; 32])
}
