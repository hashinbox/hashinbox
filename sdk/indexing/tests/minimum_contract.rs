//! The minimum a store must implement, proven from the outside.
//!
//! One type, [`Sink`], implements [`WatchStore`] and [`IndexStore`] and nothing
//! else: no [`indexing::BalanceStore`], no [`indexing::ConfirmedHistory`], no
//! [`indexing::OutputStore`], no [`indexing::Purging`]. It is deliberately **not
//! `Clone`** — it owns a channel that cannot be duplicated — so the only way it
//! reaches an index is `Arc<Sink>`, through the forwarding impls the ports
//! carry.
//!
//! What this file is for: if any of that stops being true, a third party's own
//! index sink stops compiling, and no test inside `indexing` would notice. Every
//! assertion reads the sink's own rows, never a port the sink does not
//! implement, because a third party's query vocabulary is its own business.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc::{Receiver, Sender, channel},
};

use futures_executor::block_on;
use indexing::{
    Address, Birthday, BlockHash, BlockHeight, BlockInterpreter, BlockParent, BlockPosition,
    BlockRef, BlockSource, BoxFuture, ChainName, ChainTips, FeeSettlement, ForwardRange,
    IndexError, IndexErrorKind, IndexStore, IndexedBatch, Indexer, Indexing, InterpretedBlock,
    NativeBlock, Network, OutputKey, SourceError, Subject, SyncConfig, SyncPolicy, Watch,
    WatchProgress, WatchStore, Watching,
};

/// A third party's index sink: the two required ports over two maps, plus a
/// channel it reports commits on.
///
/// The channel is the point of it being here: it is a resource that cannot be
/// cloned, which is what an application's own sink routinely owns — a connection,
/// a transaction handle, a queue producer. A store contract that required
/// [`Clone`] would make this type unusable.
struct Sink {
    watches: Mutex<Vec<Watch>>,
    /// How far each observation has been inspected, by its own identity.
    progress: Mutex<Vec<(String, BlockRef)>>,
    checkpoint: Mutex<Option<BlockRef>>,
    /// Every block this sink committed, in order, reported to whoever holds the
    /// receiver. Not `Clone`, by construction.
    committed: Sender<BlockRef>,
}

impl Sink {
    fn new() -> (Arc<Self>, Receiver<BlockRef>) {
        let (committed, reported) = channel();
        let sink = Self {
            watches: Mutex::new(Vec::new()),
            progress: Mutex::new(Vec::new()),
            checkpoint: Mutex::new(None),
            committed,
        };
        (Arc::new(sink), reported)
    }

    /// The sink's own vocabulary, which is not a port and answers to nothing in
    /// `indexing`: how far it thinks it has got.
    fn line(&self) -> Option<BlockRef> {
        self.checkpoint.lock().expect("checkpoint").clone()
    }

    /// What it was told to observe, in the order it was told.
    fn observed(&self) -> Vec<Watch> {
        self.watches.lock().expect("watches").clone()
    }
}

impl WatchStore for Sink {
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let mut held = self.watches.lock().expect("watches");
            for watch in &watches {
                let stored = held.iter().find(|stored| stored.id == watch.id);
                if stored.is_some_and(|stored| stored != watch) {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "this watch id is already registered",
                        false,
                    ));
                }
                if stored.is_none() {
                    held.push(watch.clone());
                }
            }
            Ok(())
        })
    }

    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        Box::pin(async move {
            let progress = self.progress.lock().expect("progress");
            Ok(self
                .watches
                .lock()
                .expect("watches")
                .iter()
                .filter(|watch| watch.address.belongs_to(network))
                .map(|watch| WatchProgress {
                    indexed_through: progress
                        .iter()
                        .find(|(id, _)| id == &watch.id)
                        .map(|(_, block)| block.clone()),
                    watch: watch.clone(),
                })
                .collect())
        })
    }

    fn forget<'a>(
        &'a self,
        _network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            self.watches
                .lock()
                .expect("watches")
                .retain(|stored| stored.id != watch.id);
            self.progress
                .lock()
                .expect("progress")
                .retain(|(id, _)| id != &watch.id);
            Ok(())
        })
    }

    /// The minimum a sink owes for a cancelled backfill: the row stays, its
    /// birthday moves, and the claim it earned under the old one goes.
    fn cancel_backfill<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let mut held = self.watches.lock().expect("watches");
            let mut progress = self.progress.lock().expect("progress");
            for watch in watches {
                let Some(stored) = held.iter_mut().find(|stored| stored.id == watch.id) else {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "a re-anchored observation is not registered",
                        false,
                    ));
                };
                stored.start = watch.start;
                progress.retain(|(id, _)| id != &watch.id);
            }
            Ok(())
        })
    }
}

impl IndexStore for Sink {
    fn checkpoint<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { Ok(self.line()) })
    }

    /// One write, both directions: the blocks, the claims they prove, and the
    /// line they leave behind, all or nothing.
    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move {
            let mut checkpoint = self.checkpoint.lock().expect("checkpoint");
            if checkpoint.as_ref() != batch.expected_checkpoint() {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "checkpoint changed before commit",
                    true,
                ));
            }
            let mut progress = self.progress.lock().expect("progress");
            for claim in batch.progress() {
                let held = progress.iter().position(|(id, _)| id == &claim.watch_id);
                let stored = held.map(|at| progress[at].1.clone());
                if stored != claim.expected {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "recorded progress changed before this commit",
                        true,
                    ));
                }
                match held {
                    Some(at) => progress[at].1 = claim.indexed_through.clone(),
                    None => progress.push((claim.watch_id.clone(), claim.indexed_through.clone())),
                }
            }
            for canonical in batch.blocks() {
                let block = canonical.block();
                if batch
                    .expected_checkpoint()
                    .is_some_and(|line| block.position <= line.position)
                {
                    // Already committed: this batch is filling it in for an
                    // observation it was not inspected for. A sink that kept rows
                    // would insert-or-verify them here.
                    continue;
                }
                // A sink is free to react inside its own commit. This one
                // reports; an application's would enqueue, and would own the
                // delivery.
                let _ = self.committed.send(block.clone());
            }
            if let Some(line) = batch.checkpoint() {
                *checkpoint = Some(line.clone());
            }
            Ok(Vec::new())
        })
    }
}

/// A chain of `tip + 1` dense blocks, counting every block it serves.
///
/// The count is the only way to see the difference between an index that trusted
/// the progress its store handed back and one that re-read the range anyway:
/// nothing the store answers afterwards distinguishes them.
struct Source {
    tip: u64,
    served: Arc<AtomicUsize>,
}

impl Source {
    fn to(tip: u64) -> Self {
        Self {
            tip,
            served: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The same chain, sharing one count with `self`.
    fn again(&self, tip: u64) -> Self {
        Self {
            tip,
            served: Arc::clone(&self.served),
        }
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::Acquire)
    }
}

/// The native block of this fixture chain: a reference and nothing else.
#[derive(Clone, Debug)]
struct Native(BlockRef);

impl NativeBlock for Native {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

impl ChainTips for Source {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move { Ok(block(self.tip)) })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move { Ok(block(self.tip)) })
    }
}

impl BlockSource for Source {
    type Block = Native;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Native>, SourceError>> {
        Box::pin(async move {
            let blocks: Vec<_> = (start.0..=end.0.min(self.tip))
                .take(limit)
                .map(|height| Native(block(height)))
                .collect();
            self.served.fetch_add(blocks.len(), Ordering::AcqRel);
            Ok(blocks)
        })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        Box::pin(async move { Ok((position.0 <= self.tip).then(|| block(position.0))) })
    }
}

/// Reports nothing at all: what is being proven is the storage contract, and an
/// empty block still has to commit and still has to advance the watch.
struct Interpreter;

impl BlockInterpreter for Interpreter {
    type Block = Native;

    fn inspect(
        &self,
        block: &Native,
        _addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        InterpretedBlock::new(&network(), block.0.clone(), Vec::new())
    }

    fn settlement(&self) -> FeeSettlement {
        FeeSettlement::Charged
    }
}

fn network() -> Network {
    Network::new(ChainName::new("minimal"), "net")
}

fn block(height: u64) -> BlockRef {
    let hash = |height: u64| BlockHash(height.to_be_bytes().to_vec());
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: hash(height),
        parent: height.checked_sub(1).map(|parent| BlockParent {
            position: BlockPosition(parent),
            hash: hash(parent),
        }),
        timestamp: None,
    }
}

fn subject(address: &str) -> Subject {
    Subject {
        address: Address::new(network(), address),
        asset: indexing::AssetId::native(network().chain),
    }
}

/// One index over `sink`, exactly as a third party assembles one: the chain's
/// two halves, the store behind an `Arc`, and a policy.
fn indexer(sink: &Arc<Sink>, tip: u64) -> Indexer<Source, Interpreter, Arc<Sink>> {
    over(sink, Source::to(tip), network())
}

/// The same, over a named source and network, so a scenario can keep the source
/// to read its counters or bind a second network to one sink.
fn over(
    sink: &Arc<Sink>,
    source: Source,
    network: Network,
) -> Indexer<Source, Interpreter, Arc<Sink>> {
    Indexer::new(
        source,
        Interpreter,
        Arc::clone(sink),
        SyncConfig::new(&network, SyncPolicy::new(100)).expect("sync policy"),
    )
}

/// **The completion test.** A store that answers no query at all observes
/// wallets with birthdays, runs a bounded pass, commits, and comes back after a
/// restart — and the assertions are all in the sink's own vocabulary.
#[test]
fn a_store_with_no_read_methods_observes_commits_and_restarts() {
    let (sink, reported) = Sink::new();
    let index = indexer(&sink, 3);

    // Observation, with a birthday, before anything is indexed.
    let watch = block_on(index.watch("payer", &subject("payer"), Birthday::At(BlockPosition(0))))
        .expect("an observation the store accepted");
    assert_eq!(watch.start, BlockPosition(0));
    assert_eq!(
        sink.observed(),
        std::slice::from_ref(&watch),
        "the observation is durable before the call returns",
    );

    // One bounded pass. Nothing schedules it and nothing loops.
    let status = block_on(Indexing::sync(&index)).expect("one bounded pass");
    assert_eq!(status.network, network(), "one network, one status");
    assert_eq!(
        status.checkpoint.as_ref(),
        Some(&block(3)),
        "the pass committed to the confirmed tip",
    );
    assert_eq!(sink.line().as_ref(), Some(&block(3)));
    let committed: Vec<_> = reported.try_iter().collect();
    assert_eq!(
        committed,
        (0..=3).map(block).collect::<Vec<_>>(),
        "every block reached the sink's own commit path, empty ones included",
    );

    // A second pass over an unchanged chain is a no-op rather than a rewrite.
    block_on(Indexing::sync(&index)).expect("a second bounded pass");
    assert!(
        reported.try_iter().next().is_none(),
        "nothing was committed twice",
    );

    // Restart: a new index over the same store, with nothing re-watched.
    drop(index);
    let restarted = indexer(&sink, 5);
    assert_eq!(
        block_on(restarted.watched(&network())).expect("the restored watch list"),
        [watch],
        "the restart read its observations back out of the sink",
    );
    let status = block_on(Indexing::sync(&restarted)).expect("a pass after the restart");
    assert_eq!(status.checkpoint.as_ref(), Some(&block(5)));
    assert_eq!(
        reported.try_iter().collect::<Vec<_>>(),
        [block(4), block(5)],
        "it resumed from the line the sink held, and did not re-commit below it",
    );
}

/// The watch list is the sink's, and withdrawing an observation reaches it.
#[test]
fn withdrawing_an_observation_reaches_the_store() {
    let (sink, _reported) = Sink::new();
    let index = indexer(&sink, 1);
    block_on(index.watch("payer", &subject("payer"), Birthday::At(BlockPosition(0))))
        .expect("an observation");

    let withdrawn = block_on(index.unwatch("payer")).expect("a withdrawal");

    assert_eq!(withdrawn.map(|watch| watch.id).as_deref(), Some("payer"));
    assert!(
        sink.observed().is_empty(),
        "the durable half of an unwatch is the store's",
    );
    assert!(
        block_on(index.unwatch("payer"))
            .expect("withdrawing again is not an error")
            .is_none(),
    );
}

/// A `Watch` the store already holds under a different definition is refused,
/// and the refusal is the store's own: the index does not police it twice.
#[test]
fn the_store_refuses_a_redefinition_of_an_identity_it_holds() {
    let (sink, _reported) = Sink::new();
    block_on(sink.add(vec![Watch {
        id: "payer".to_owned(),
        address: Address::new(network(), "payer"),
        asset: indexing::AssetId::native(network().chain),
        start: BlockPosition(7),
    }]))
    .expect("the first observation");

    let error = block_on(sink.add(vec![Watch {
        id: "payer".to_owned(),
        address: Address::new(network(), "elsewhere"),
        asset: indexing::AssetId::native(network().chain),
        start: BlockPosition(7),
    }]))
    .expect_err("a redefinition");

    assert_eq!(error.kind, IndexErrorKind::Conflict);
    assert_eq!(sink.observed().len(), 1);
}

/// **The mutation this exists for.** An index that ignored the progress its store
/// handed back would look identical from the store's side: it would re-read the
/// same range, write the same rows, and record the same claim. What tells the two
/// apart is the *source*, so this counts the blocks it served.
///
/// Take the store's answer away — have [`WatchStore::all`] drop every
/// `indexed_through` — and the second half of this test fails, because the
/// restarted index reads the range below the checkpoint again.
#[test]
fn a_restart_trusts_the_progress_the_store_recorded() {
    let (sink, _reported) = Sink::new();
    // Three blocks with nothing observed: a checkpoint, and a range nothing was
    // inspected for.
    let first = Source::to(3);
    let index = over(&sink, first.again(3), network());
    block_on(Indexing::sync(&index)).expect("the forward pass");
    let forward = first.served();

    // An observation born below the checkpoint owes that range, and the next
    // pass reads it.
    block_on(index.watch("payer", &subject("payer"), Birthday::At(BlockPosition(0))))
        .expect("an observation below the line");
    block_on(Indexing::sync(&index)).expect("the pass that fills the range in");
    let filled = first.served();
    assert!(
        filled > forward,
        "the pass read the range below the checkpoint: {forward} then {filled}",
    );
    assert!(
        block_on(sink.all(&network()))
            .expect("observations")
            .into_iter()
            .all(|observed| observed.caught_up(sink.line().as_ref())),
        "the store recorded a claim that reaches the line",
    );

    // The restart has the same store and the same chain. It must read only what
    // is new.
    drop(index);
    let restarted = over(&sink, first.again(5), network());
    block_on(Indexing::sync(&restarted)).expect("the pass after the restart");

    assert_eq!(
        first.served() - filled,
        2,
        "only blocks 4 and 5 were new; a restart that re-derived its own \
         progress would have read 0 to 3 again, and nothing the store answers \
         afterwards would show it",
    );
}

/// One store, two networks: an index of one never observes the other's rows.
///
/// [`WatchStore::all`] takes the network for a reason, and a sink that ignores it
/// would pass every single-network test there is. What breaks here is the watch
/// set: the index of `network()` would restore an address it cannot inspect, and
/// report it as watched.
///
/// **The restart in the middle is load-bearing, and was missing.** `Watches`
/// mirrors the store in memory and reads it exactly once, so an index that
/// registered its own observations never consults `all` again -- and this
/// scenario passed with the network filter deleted until the two indexes were
/// dropped and rebuilt here.
#[test]
fn an_index_of_one_network_reads_only_that_network_out_of_a_shared_store() {
    let elsewhere = Network::new(ChainName::new("minimal"), "other");
    let (sink, _reported) = Sink::new();
    let mine = indexer(&sink, 1);
    let theirs = over(&sink, Source::to(1), elsewhere.clone());

    block_on(mine.watch("here", &subject("payer"), Birthday::At(BlockPosition(0))))
        .expect("an observation of this network");
    block_on(theirs.watch(
        "there",
        &Subject {
            address: Address::new(elsewhere.clone(), "payer"),
            asset: indexing::AssetId::native(elsewhere.chain.clone()),
        },
        Birthday::At(BlockPosition(0)),
    ))
    .expect("an observation of the other network");

    // Both indexes go away before the watch lists are read, so each list comes
    // out of the shared store rather than out of a hot set that only ever saw
    // its own writes. Without the restart this scenario passes with
    // `WatchStore::all`'s network filter deleted -- `Watches::load` reads the
    // store once, on an index whose store is still empty, and never again.
    drop(mine);
    drop(theirs);
    let mine = indexer(&sink, 1);
    let theirs = over(&sink, Source::to(1), elsewhere.clone());

    assert_eq!(
        block_on(mine.watched(&network()))
            .expect("this network's watch list")
            .into_iter()
            .map(|watch| watch.id)
            .collect::<Vec<_>>(),
        ["here"],
    );
    assert_eq!(
        block_on(theirs.watched(&elsewhere))
            .expect("the other network's watch list")
            .into_iter()
            .map(|watch| watch.id)
            .collect::<Vec<_>>(),
        ["there"],
        "two indexes over one store keep their own selections",
    );
    assert_eq!(
        sink.observed().len(),
        2,
        "both observations are in the one store they share",
    );
}

/// A minimal sink is a [`indexing::Repository`] by the blanket impl, and so is
/// an `Arc` of one: what an index asks for is exactly those two ports.
#[test]
fn the_two_ports_are_the_whole_requirement() {
    fn requires_a_repository<R: indexing::Repository>(_store: &R) {}
    fn requires_a_filter(filter: ForwardRange) -> BlockPosition {
        filter.start_position
    }

    let (sink, _reported) = Sink::new();
    requires_a_repository(&sink);
    requires_a_repository(sink.as_ref());
    assert_eq!(
        requires_a_filter(ForwardRange {
            address: Address::new(network(), "payer"),
            start_position: BlockPosition(1),
        }),
        BlockPosition(1),
    );
}
