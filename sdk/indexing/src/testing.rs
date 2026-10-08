//! A scripted chain double for tests of code that drives an
//! [`Indexer`](crate::Indexer): one [`Chain`] that is the chain's
//! [`ChainTips`], [`BlockSource`] and [`BlockInterpreter`] at once.
//!
//! The test states the chain's blocks (any positions, any linkage), how far
//! below its tip it confirms, what each block holds, and the [`Fault`]s it
//! answers with. The chain records every [`Request`] it answered, every block
//! it served, and every inspection it made, so a scenario can prove what an
//! index read and for whom.
//!
//! A block's facts are keyed by its hash, so a fork block at the same height
//! holds nothing until it is told to. Inspection reports what a chain would:
//! the transactions, output changes and credits that touch a selected address.
//!
//! Clones share one chain, so a test keeps one to script and hands another to
//! the index. Behind the `testing` feature, which is off by default and belongs
//! in a `[dev-dependencies]` entry only.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use crate::{
    Address, BlockCredit, BlockHash, BlockHeight, BlockInterpreter, BlockParent, BlockPosition,
    BlockRef, BlockSource, BlockWithOutputs, BoxFuture, Budget, ChainTips, Config, FeeSettlement,
    IndexError, Indexer, Indexers, Indexing, NativeBlock, Network, OutputChanges, Pending,
    SourceError, Transaction,
};

/// The block at `height` of a dense chain: position and height equal, the
/// height's big-endian bytes as its hash, the block below as its parent, and
/// no timestamp.
#[must_use]
pub fn block(height: u64) -> BlockRef {
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

/// The native block of the double: its reference and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block(BlockRef);

impl NativeBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

/// One call the chain answered, in the words of the trait that made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// [`ChainTips::tip`].
    Tip,
    /// [`ChainTips::confirmed_tip`].
    ConfirmedTip,
    /// [`BlockSource::blocks`], with its arguments.
    Blocks {
        /// The first position asked for.
        start: BlockPosition,
        /// The last position asked for.
        end: BlockPosition,
        /// The most blocks asked for.
        limit: usize,
    },
    /// [`BlockSource::canonical_at`], at this position.
    CanonicalAt(BlockPosition),
}

/// A way the chain misbehaves until [`Chain::heal`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Every fetch that starts below `below` is refused, the way a pruned node
    /// refuses it, with this retryability.
    Pruned {
        /// The oldest position the node still holds.
        below: BlockPosition,
        /// Whether the refusal is retryable.
        retryable: bool,
    },
    /// Every fetch leaves out the block at this position, while
    /// [`BlockSource::canonical_at`] still answers for it.
    Omitted(BlockPosition),
    /// The canonical lookup at this position fails, retryably.
    Unavailable(BlockPosition),
    /// Every fetch answers exactly these blocks, whatever range and limit it
    /// asked for.
    Unbounded(Vec<BlockRef>),
    /// Inspection reports each block under its reference with the timestamp
    /// moved to one: the same block, misreported.
    Relabelled,
}

/// What one block holds, before a selection narrows it.
#[derive(Clone, Default)]
struct Facts {
    transactions: Vec<Transaction>,
    outputs: OutputChanges,
    credits: Vec<BlockCredit>,
}

impl Facts {
    /// What a chain reports of these facts for `addresses`: everything that
    /// touches one of them, in the order it was scripted.
    fn selected(&self, addresses: &[Address]) -> Self {
        let selected = |address: &Address| addresses.contains(address);
        Self {
            transactions: self
                .transactions
                .iter()
                .filter(|draft| draft.addresses().iter().any(selected))
                .cloned()
                .collect(),
            outputs: OutputChanges {
                created: (self.outputs.created.iter())
                    .filter(|output| selected(&output.address))
                    .cloned()
                    .collect(),
                spent: (self.outputs.spent.iter())
                    .filter(|key| selected(&key.address))
                    .cloned()
                    .collect(),
                tracked_spends: (self.outputs.tracked_spends.iter())
                    .filter(|key| selected(&key.address))
                    .cloned()
                    .collect(),
            },
            credits: (self.credits.iter())
                .filter(|credit| selected(&credit.to))
                .cloned()
                .collect(),
        }
    }
}

/// Runs after each request is answered and before the answer is returned.
type Hook = Arc<dyn Fn(&Request) + Send + Sync>;

#[derive(Default)]
struct State {
    blocks: BTreeMap<BlockPosition, BlockRef>,
    facts: BTreeMap<BlockHash, Facts>,
    depth: u64,
    confirmed: Option<BlockRef>,
    faults: Vec<Fault>,
    hook: Option<Hook>,
    requests: Vec<Request>,
    fetched: Vec<BlockHeight>,
    inspections: Vec<(BlockHeight, Vec<Address>)>,
}

impl State {
    fn tip(&self) -> Result<BlockRef, SourceError> {
        self.blocks
            .last_key_value()
            .map(|(_, block)| block.clone())
            .ok_or_else(|| SourceError::new("source has no produced blocks", true))
    }

    /// The stated line, or the newest block `depth` heights below the tip.
    fn confirmed_tip(&self) -> Result<BlockRef, SourceError> {
        if let Some(line) = &self.confirmed {
            return Ok(line.clone());
        }
        let line = self.tip()?.height.0.saturating_sub(self.depth);
        self.blocks
            .values()
            .rfind(|block| block.height.0 <= line)
            .cloned()
            .ok_or_else(|| SourceError::new("source has no confirmed block", true))
    }

    fn blocks(
        &mut self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> Result<Vec<Block>, SourceError> {
        if limit == 0 || start > end {
            return Err(SourceError::new("invalid block range", false));
        }
        let mut omitted = Vec::new();
        for fault in &self.faults {
            match fault {
                Fault::Pruned { below, retryable } if start < *below => {
                    let message = "block is not available on this node";
                    return Err(SourceError::new(message, *retryable));
                }
                Fault::Unbounded(answer) => {
                    return Ok(answer.iter().cloned().map(Block).collect());
                }
                Fault::Omitted(position) => omitted.push(*position),
                _ => {}
            }
        }
        let served: Vec<_> = (self.blocks.range(start..=end))
            .filter(|(position, _)| !omitted.contains(position))
            .take(limit)
            .map(|(_, block)| Block(block.clone()))
            .collect();
        self.fetched
            .extend(served.iter().map(|block| block.0.height));
        Ok(served)
    }

    fn canonical_at(&self, position: BlockPosition) -> Result<Option<BlockRef>, SourceError> {
        if self.faults.contains(&Fault::Unavailable(position)) {
            return Err(SourceError::new("canonical lookup unavailable", true));
        }
        Ok(self.blocks.get(&position).cloned())
    }

    /// Records the inspection and returns the reference to report and the
    /// facts `addresses` select.
    fn inspect(&mut self, block: &BlockRef, addresses: &[Address]) -> (BlockRef, Facts) {
        self.inspections.push((block.height, addresses.to_vec()));
        let facts = (self.facts.get(&block.hash))
            .map(|facts| facts.selected(addresses))
            .unwrap_or_default();
        let mut reported = block.clone();
        if self.faults.contains(&Fault::Relabelled) {
            reported.timestamp = Some(1);
        }
        (reported, facts)
    }
}

/// One scripted chain of one network: its blocks, its confirmed line, what
/// each block holds, and how it fails. Clones share it.
#[derive(Clone)]
pub struct Chain {
    network: Network,
    settlement: FeeSettlement,
    state: Arc<Mutex<State>>,
}

impl Chain {
    /// A chain of `network` holding exactly `blocks`, keyed by position, that
    /// confirms its tip and settles fees [`FeeSettlement::Charged`].
    #[must_use]
    pub fn new(network: Network, blocks: impl IntoIterator<Item = BlockRef>) -> Self {
        let chain = Self {
            network,
            settlement: FeeSettlement::Charged,
            state: Arc::default(),
        };
        chain.replace(blocks);
        chain
    }

    /// A chain of `network` holding [`block`] `0..=tip`.
    #[must_use]
    pub fn dense(network: Network, tip: u64) -> Self {
        Self::new(network, (0..=tip).map(block))
    }

    /// The same chain, confirming the newest block `depth` heights below its
    /// tip.
    #[must_use]
    pub fn confirming(self, depth: u64) -> Self {
        self.state().depth = depth;
        self
    }

    /// The same chain, stating `settlement` for its fees.
    #[must_use]
    pub const fn settling(mut self, settlement: FeeSettlement) -> Self {
        self.settlement = settlement;
        self
    }

    /// The network every block this chain reports is stated to be of.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// Replaces every block with `blocks`: a reorg, a pruned node, or any
    /// shape a scenario needs. Facts stay with the hashes they were told for.
    pub fn replace(&self, blocks: impl IntoIterator<Item = BlockRef>) {
        self.state().blocks = blocks
            .into_iter()
            .map(|block| (block.position, block))
            .collect();
    }

    /// Produces [`block`]s above the newest height through `tip`; nothing
    /// when the chain already reaches it.
    pub fn extend_to(&self, tip: u64) {
        let mut state = self.state();
        let next = (state.blocks.last_key_value())
            .map_or(Some(0), |(_, newest)| newest.height.0.checked_add(1));
        let Some(next) = next else {
            return;
        };
        for height in next..=tip {
            state.blocks.insert(BlockPosition(height), block(height));
        }
    }

    /// States `line` as the confirmed tip from now on, whatever the blocks
    /// and the depth say: a line that walks backwards or runs ahead.
    pub fn confirm(&self, line: BlockRef) {
        self.state().confirmed = Some(line);
    }

    /// Adds `draft` to what `block` holds.
    pub fn include(&self, block: &BlockRef, draft: Transaction) {
        self.facts(block, |facts| facts.transactions.push(draft));
    }

    /// Adds `changes` to the output changes `block` holds.
    pub fn change_outputs(&self, block: &BlockRef, changes: OutputChanges) {
        self.facts(block, |facts| {
            let outputs = &mut facts.outputs;
            outputs.created.extend(changes.created);
            outputs.spent.extend(changes.spent);
            outputs.tracked_spends.extend(changes.tracked_spends);
        });
    }

    /// Adds `credit` to the value `block` moves with no transaction.
    pub fn credit(&self, block: &BlockRef, credit: BlockCredit) {
        self.facts(block, |facts| facts.credits.push(credit));
    }

    /// Answers with `fault` until [`Self::heal`].
    pub fn fail(&self, fault: Fault) {
        self.state().faults.push(fault);
    }

    /// Clears every fault.
    pub fn heal(&self) {
        self.state().faults.clear();
    }

    /// Runs `hook` after every later request is answered and before its
    /// answer is returned, outside the chain's lock: the round trip during
    /// which anything else may happen. A hook may hold a clone of the chain
    /// to change it; the two then live as long as each other, which a test
    /// never notices.
    pub fn on_request(&self, hook: impl Fn(&Request) + Send + Sync + 'static) {
        self.state().hook = Some(Arc::new(hook));
    }

    /// Every request answered so far, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.state().requests.clone()
    }

    /// The height of every block fetches served so far, in serving order.
    #[must_use]
    pub fn fetched(&self) -> Vec<BlockHeight> {
        self.state().fetched.clone()
    }

    /// Every block inspected so far and the addresses it was inspected for.
    #[must_use]
    pub fn inspections(&self) -> Vec<(BlockHeight, Vec<Address>)> {
        self.state().inspections.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A test that panicked while holding the lock already failed; the
        // chain it scripted is still the truth about what was asked.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn facts(&self, block: &BlockRef, change: impl FnOnce(&mut Facts)) {
        change(self.state().facts.entry(block.hash.clone()).or_default());
    }

    /// Records `request`, answers it from the state, then runs the hook.
    fn answer<T>(&self, request: Request, read: impl FnOnce(&mut State) -> T) -> T {
        let (answer, hook) = {
            let mut state = self.state();
            state.requests.push(request.clone());
            (read(&mut state), state.hook.clone())
        };
        if let Some(hook) = hook {
            hook(&request);
        }
        answer
    }
}

impl ChainTips for Chain {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        let answer = self.answer(Request::Tip, |state| state.tip());
        Box::pin(async move { answer })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        let answer = self.answer(Request::ConfirmedTip, |state| state.confirmed_tip());
        Box::pin(async move { answer })
    }
}

impl BlockSource for Chain {
    type Block = Block;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Block>, SourceError>> {
        let request = Request::Blocks { start, end, limit };
        let answer = self.answer(request, |state| state.blocks(start, end, limit));
        Box::pin(async move { answer })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        let request = Request::CanonicalAt(position);
        let answer = self.answer(request, |state| state.canonical_at(position));
        Box::pin(async move { answer })
    }
}

impl BlockInterpreter for Chain {
    type Block = Block;

    fn inspect(
        &self,
        block: &Block,
        addresses: &[Address],
    ) -> Result<BlockWithOutputs, IndexError> {
        let (reference, facts) = self.state().inspect(&block.0, addresses);
        Ok(BlockWithOutputs {
            outputs: facts.outputs,
            credits: facts.credits,
            ..BlockWithOutputs::new(&self.network, reference, facts.transactions)?
        })
    }

    fn settlement(&self) -> FeeSettlement {
        self.settlement
    }
}

/// Several networks, each a scripted [`Chain`], indexed by real indexers into
/// the one repository the caller supplies, each network's pending blocks
/// handed out beside the index the way an application composes production.
///
/// What a consumer's own tests run on when they need real answers behind a
/// route or a report: script the chains, [`Indexers::sync`], then read.
pub struct Networks {
    chains: Vec<Chain>,
    index: Arc<Indexers>,
    pending: Vec<(Network, Arc<dyn Pending>)>,
}

impl Networks {
    /// Every one of `networks` as a dense chain at `tip`, confirming `depth`
    /// blocks deep, indexed into `repository`. The caller keeps a clone of it
    /// for whatever else it reads or purges.
    ///
    /// # Panics
    ///
    /// When two of `networks` are the same network.
    #[must_use]
    pub fn new<R>(networks: &[Network], tip: u64, depth: u64, repository: R) -> Self
    where
        R: crate::Repository + Clone + Send + Sync + 'static,
    {
        let mut chains = Vec::new();
        let mut indexers: Vec<Arc<dyn Indexing>> = Vec::new();
        let mut pending: Vec<(Network, Arc<dyn Pending>)> = Vec::new();
        for network in networks {
            let chain = Chain::dense(network.clone(), tip).confirming(depth);
            // A short backfill bound, so a too-old start is refusable in tests.
            let policy = Budget::new(100).with_backfill(3);
            let config = Config::new(network, policy).expect("a valid policy");
            let indexer = Arc::new(Indexer::new(
                chain.clone(),
                chain.clone(),
                repository.clone(),
                config,
            ));
            pending.push((network.clone(), indexer.clone()));
            indexers.push(indexer);
            chains.push(chain);
        }
        Self {
            chains,
            index: Arc::new(Indexers::of(indexers).expect("distinct networks")),
            pending,
        }
    }

    /// The composed index of every network.
    #[must_use]
    pub fn index(&self) -> Arc<Indexers> {
        self.index.clone()
    }

    /// Every network's blocks above its confirmed line, in the order given.
    #[must_use]
    pub fn pending(&self) -> Vec<(Network, Arc<dyn Pending>)> {
        self.pending.clone()
    }

    /// Every network's scripted chain, in the order given.
    #[must_use]
    pub fn chains(&self) -> &[Chain] {
        &self.chains
    }
}
