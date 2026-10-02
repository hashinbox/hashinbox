//! Contract coverage for `Indexer`: bounded sync over sparse positions, birthdays,
//! reorgs, and selection validation, over in-memory sources and stores.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use futures_executor::block_on;
use indexing::{
    Address, Birthday, BlockHash, BlockHeight, BlockInterpreter, BlockParent, BlockPosition,
    BlockRef, BlockSource, BoxFuture, CanonicalBlock, CanonicalPage, ChainName, ChainTips,
    Checkpoint, ConfirmedHistory, ForwardRange, HistoryQuery, IndexError, IndexErrorKind,
    IndexStore, IndexedOutput, Indexer, Indexing, InterpretedBlock, NativeBlock, Network,
    OutputChanges, OutputId, OutputKey, Pending, Purging, SourceError, Subject, SyncConfig,
    SyncPhase, SyncPolicy, SyncStatus, TransactionRef, Watching, memory,
};

#[derive(Clone, Debug)]
struct TestBlock(BlockRef);

impl NativeBlock for TestBlock {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

#[derive(Clone)]
struct Source {
    blocks: Arc<Mutex<BTreeMap<BlockPosition, TestBlock>>>,
    tip_calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<BlockHeight>>>,
    canonical_requests: Arc<Mutex<Vec<BlockPosition>>>,
    canonical_failure: Arc<Mutex<Option<BlockPosition>>>,
    /// How many blocks below the tip this source calls irreversible. Zero is
    /// a chain with instant finality, which is what most scenarios want.
    depth: u64,
    /// Runs while the tip is being observed, standing in for anything that can
    /// happen during that round trip — such as a wallet being created.
    during_tip: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Source {
    fn new(tip: u64) -> Self {
        let blocks = (0..=tip)
            .map(|height| {
                let block = TestBlock(block(BlockHeight(height)));
                (block.0.position, block)
            })
            .collect();
        Self {
            blocks: Arc::new(Mutex::new(blocks)),
            tip_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            canonical_requests: Arc::new(Mutex::new(Vec::new())),
            canonical_failure: Arc::new(Mutex::new(None)),
            depth: 0,
            during_tip: None,
        }
    }

    fn sparse(blocks: impl IntoIterator<Item = BlockRef>) -> Self {
        Self {
            blocks: Arc::new(Mutex::new(
                blocks
                    .into_iter()
                    .map(|block| (block.position, TestBlock(block)))
                    .collect(),
            )),
            tip_calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            canonical_requests: Arc::new(Mutex::new(Vec::new())),
            canonical_failure: Arc::new(Mutex::new(None)),
            depth: 0,
            during_tip: None,
        }
    }

    /// The same source, confirming `depth` blocks below its tip.
    const fn confirming(mut self, depth: u64) -> Self {
        self.depth = depth;
        self
    }

    fn replace(&self, blocks: impl IntoIterator<Item = BlockRef>) {
        *self.blocks.lock().expect("source blocks") = blocks
            .into_iter()
            .map(|block| (block.position, TestBlock(block)))
            .collect();
    }

    fn during_tip(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.during_tip = Some(Arc::new(hook));
        self
    }

    fn tip_calls(&self) -> usize {
        self.tip_calls.load(Ordering::Acquire)
    }

    fn requests(&self) -> Vec<BlockHeight> {
        self.requests.lock().expect("source requests").clone()
    }
}

impl ChainTips for Source {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.tip_calls.fetch_add(1, Ordering::AcqRel);
        if let Some(hook) = &self.during_tip {
            hook();
        }
        let result = self
            .blocks
            .lock()
            .expect("source blocks")
            .last_key_value()
            .map(|(_, block)| block.0.clone())
            .ok_or_else(|| SourceError {
                message: "source has no produced blocks".into(),
                retryable: true,
            });
        Box::pin(async move { result })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        let blocks = self.blocks.lock().expect("source blocks");
        let result = blocks
            .last_key_value()
            .map(|(_, block)| block.0.height.0.saturating_sub(self.depth))
            .and_then(|line| {
                blocks
                    .values()
                    .rfind(|block| block.0.height.0 <= line)
                    .map(|block| block.0.clone())
            })
            .ok_or_else(|| SourceError {
                message: "source has no confirmed block".into(),
                retryable: true,
            });
        drop(blocks);
        Box::pin(async move { result })
    }
}

impl BlockSource for Source {
    type Block = TestBlock;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        let result = if limit == 0 || start > end {
            Err(SourceError {
                message: "invalid block range".into(),
                retryable: false,
            })
        } else {
            let blocks = self
                .blocks
                .lock()
                .expect("source blocks")
                .range(start..=end)
                .take(limit)
                .map(|(_, block)| block.clone())
                .collect::<Vec<_>>();
            let mut requests = self.requests.lock().expect("source requests");
            for block in &blocks {
                requests.push(block.0.height);
            }
            Ok(blocks)
        };
        Box::pin(async move { result })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        self.canonical_requests.lock().unwrap().push(position);
        if *self.canonical_failure.lock().unwrap() == Some(position) {
            return Box::pin(async {
                Err(SourceError {
                    message: "canonical lookup unavailable".into(),
                    retryable: true,
                })
            });
        }
        let block = self
            .blocks
            .lock()
            .expect("source blocks")
            .get(&position)
            .map(|block| block.0.clone());
        Box::pin(async move { Ok(block) })
    }
}

#[derive(Clone, Default)]
struct Repository {
    state: Arc<Mutex<RepositoryState>>,
}

#[derive(Default)]
struct RepositoryState {
    checkpoint: Option<BlockRef>,
    blocks: BTreeMap<BlockHeight, BlockRef>,
    /// Live outputs, exactly as a real backend holds them: a required spend
    /// of a key that is not here is reported, never refused.
    outputs: BTreeMap<OutputKey, IndexedOutput>,
    /// Committed history, append-only like every real backend's.
    history: Vec<indexing::CanonicalTransaction>,
    /// Every block a commit carried, in the order they arrived, exactly as they
    /// were handed over.
    committed: Vec<CanonicalBlock>,
    /// How far each observation has been inspected, keyed by its identity as
    /// every real backend keys it.
    progress: BTreeMap<String, BlockRef>,
    /// The durable watch list, keyed by identity as every real backend keys it.
    watches: BTreeMap<String, indexing::Watch>,
    /// Whether [`indexing::WatchStore::add`] persists the row and then never
    /// acknowledges it: the window its own contract names, where a cancelled
    /// future leaves the outcome uncertain rather than rolled back.
    pause_add: bool,
}

impl indexing::IndexStore for Repository {
    fn checkpoint<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .expect("repository state")
                .checkpoint
                .clone())
        })
    }

    /// One write, both directions: the blocks, the claims they prove and the line
    /// they leave behind. The full behaviour — idempotent rows, the folded output
    /// changes — is the `BackfillContract`'s subject, over the two real stores;
    /// what matters here is the checkpoint compare-and-set, the progress
    /// compare-and-set, and that a recorded claim survives, so a scenario that
    /// should cost no fetch really costs none.
    fn commit<'a>(
        &'a self,
        batch: indexing::IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("repository state");
            if state.checkpoint.as_ref() != batch.expected_checkpoint() {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "checkpoint changed",
                    true,
                ));
            }
            for claim in batch.progress() {
                if state.progress.get(&claim.watch_id) != claim.expected.as_ref() {
                    return Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "recorded progress changed",
                        true,
                    ));
                }
            }
            // Block by block, creations before spends, in the order the chain
            // built them: a batch no longer nets an output's whole life away,
            // so an output created here and spent in a later block of the same
            // batch leaves a spend behind rather than nothing.
            let mut unmatched_spends = Vec::new();
            for canonical in batch.blocks() {
                state.committed.push(canonical.clone());
                state
                    .history
                    .extend(canonical.transactions().iter().cloned());
                let block = canonical.block().clone();
                state.blocks.insert(block.height, block);
                for output in &canonical.outputs().created {
                    state.outputs.insert(output.key(), output.clone());
                }
                for key in &canonical.outputs().spent {
                    if state.outputs.remove(key).is_none() {
                        unmatched_spends.push(key.clone());
                    }
                }
            }
            for claim in batch.progress() {
                state
                    .progress
                    .insert(claim.watch_id.clone(), claim.indexed_through.clone());
            }
            if let Some(line) = batch.checkpoint() {
                state.checkpoint = Some(line.clone());
            }
            Ok(unmatched_spends)
        })
    }
}

/// The durable watch list: one map by identity, and the progress each
/// The durable watch list: one map by identity, one by address for the coverage
/// that belongs to the address rather than to any one observation.
impl indexing::WatchStore for Repository {
    fn add<'a>(&'a self, watches: Vec<indexing::Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            self.persist(watches)?;
            // Written, and never acknowledged. A caller that gives up here has a
            // durable row and no answer, which is exactly what the port says it
            // may be left with.
            if self.state.lock().expect("repository state").pause_add {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn all<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<indexing::WatchProgress>, IndexError>> {
        Box::pin(async move {
            let state = self.state.lock().expect("repository state");
            Ok(state
                .watches
                .values()
                .map(|watch| indexing::WatchProgress {
                    indexed_through: state.progress.get(&watch.id).cloned(),
                    watch: watch.clone(),
                })
                .collect())
        })
    }

    fn forget<'a>(
        &'a self,
        _network: &'a Network,
        watch: &'a indexing::Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("repository state");
            state.watches.remove(&watch.id);
            state.progress.remove(&watch.id);
            Ok(())
        })
    }

    /// The birthday moves and the claim goes, under one lock: no row leaves the
    /// map, so there is no instant at which the address is watched by nothing.
    fn cancel_backfill<'a>(
        &'a self,
        watches: Vec<indexing::Watch>,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("repository state");
            for watch in watches {
                let Some(stored) = state.watches.get_mut(&watch.id) else {
                    return Err(IndexError::new(
                        indexing::IndexErrorKind::Conflict,
                        "a re-anchored observation is not registered",
                        false,
                    ));
                };
                stored.start = watch.start;
                state.progress.remove(&watch.id);
            }
            Ok(())
        })
    }
}

impl indexing::Purging for Repository {
    /// The offline rescan: everything indexing owns for the network goes, and
    /// the observations stay.
    fn purge_network<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<indexing::PurgedRows, IndexError>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("repository state");
            let removed = indexing::PurgedRows {
                checkpoint: u64::from(state.checkpoint.is_some()),
                history: state.history.len() as u64,
                movements: 0,
                outputs: state.outputs.len() as u64,
                credits: 0,
            };
            let watches = std::mem::take(&mut state.watches);
            *state = RepositoryState {
                watches,
                ..RepositoryState::default()
            };
            Ok(removed)
        })
    }
}

impl Repository {
    /// Every committed block by height, and the current checkpoint: the whole
    /// durable state, so a test can assert that a reorg changed none of it.
    fn stored(&self) -> (BTreeMap<BlockHeight, BlockRef>, Option<BlockRef>) {
        let state = self.state.lock().expect("repository state");
        (state.blocks.clone(), state.checkpoint.clone())
    }

    /// Every block the index committed, as the store was handed it.
    fn committed(&self) -> Vec<CanonicalBlock> {
        self.state
            .lock()
            .expect("repository state")
            .committed
            .clone()
    }

    fn outputs(&self) -> BTreeSet<OutputKey> {
        self.state
            .lock()
            .expect("repository state")
            .outputs
            .keys()
            .cloned()
            .collect()
    }

    /// The durable half of a registration: every row, or the conflict it hit.
    fn persist(&self, watches: Vec<indexing::Watch>) -> Result<(), IndexError> {
        let mut state = self.state.lock().expect("repository state");
        for watch in watches {
            let held = state.watches.get(&watch.id);
            if held.is_some_and(|stored| stored != &watch) {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "this watch id is already registered",
                    false,
                ));
            }
            state.watches.insert(watch.id.clone(), watch);
        }
        Ok(())
    }

    /// How far the store recorded that `watch` was inspected.
    fn progress(&self, watch: &str) -> Option<BlockRef> {
        self.state
            .lock()
            .expect("repository state")
            .progress
            .get(watch)
            .cloned()
    }

    /// Persists every later [`indexing::WatchStore::add`] without ever
    /// acknowledging it.
    fn pause_add(&self) {
        self.state.lock().expect("repository state").pause_add = true;
    }

    /// Whether the durable list holds a row under `watch`.
    fn holds(&self, watch: &str) -> bool {
        self.state
            .lock()
            .expect("repository state")
            .watches
            .contains_key(watch)
    }
}

/// The live outputs, refused while the address is still being inspected below
/// the line they would be read at.
///
/// The refusal is the store's own, as it is in both shipped stores: the
/// observations and the rows are one lock here and one transaction there, so
/// nothing downstream has to repair the answer. `indexing::Inspection` is the
/// rule, asked the way a third party's store may ask it.
impl indexing::OutputStore for Repository {
    fn list<'a>(
        &'a self,
        request: indexing::OutputRequest,
    ) -> BoxFuture<'a, Result<indexing::OutputPage, IndexError>> {
        Box::pin(async move {
            let state = self.state.lock().expect("repository state");
            let observations = state
                .watches
                .values()
                .filter(|watch| watch.address == request.address)
                .map(|watch| indexing::WatchProgress {
                    watch: watch.clone(),
                    indexed_through: state.progress.get(&watch.id).cloned(),
                })
                .collect::<Vec<_>>();
            indexing::Inspection::new(&observations, state.checkpoint.as_ref()).spendable()?;
            Ok(indexing::OutputPage {
                checkpoint: state.checkpoint.clone(),
                outputs: state
                    .outputs
                    .values()
                    .filter(|output| output.address == request.address)
                    .cloned()
                    .collect(),
                next: None,
            })
        })
    }
}

impl indexing::BalanceStore for Repository {
    fn balance<'a>(
        &'a self,
        query: indexing::BalanceQuery,
    ) -> BoxFuture<'a, Result<indexing::Balance, IndexError>> {
        Box::pin(async move {
            let state = self.state.lock().expect("repository state");
            let mut ledger = indexing::Fold::new(&query);
            for transaction in &state.history {
                ledger.apply(transaction, indexing::FeeSettlement::Charged)?;
            }
            Ok(ledger.balance(state.checkpoint.clone()))
        })
    }
}

impl ConfirmedHistory for Repository {
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move {
            let state = self.state.lock().expect("repository state");
            Ok(CanonicalPage {
                checkpoint: state.checkpoint.clone(),
                transactions: state
                    .history
                    .iter()
                    .filter_map(|transaction| request.select(transaction.clone()))
                    .collect(),
                next: None,
                completeness: indexing::Completeness::Incomplete,
            })
        })
    }
}

type Inspections = Arc<Mutex<Vec<(BlockHeight, Vec<Address>)>>>;

#[derive(Clone)]
struct Interpreter {
    /// The network every block this interpreter reports is stated to be of.
    network: Network,
    inspections: Inspections,
    /// Output changes to report for one height, standing in for a chain whose
    /// interpreter classifies a spend of a selected address as required.
    changes: Arc<Mutex<BTreeMap<BlockHeight, OutputChanges>>>,
    /// Transactions to report for one block, keyed by its hash: a fork block
    /// at the same height is a different block and carries nothing.
    drafts: Arc<Mutex<BTreeMap<Vec<u8>, Vec<indexing::ObservationDraft>>>>,
    /// Value a block moved with no transaction to name it, keyed by its hash
    /// for the same reason. An Ethereum consensus-layer withdrawal is the one
    /// that exists.
    withdrawals: Arc<Mutex<BTreeMap<Vec<u8>, Vec<indexing::BlockCredit>>>>,
}

impl Interpreter {
    /// An interpreter of `network` that reports nothing until it is told to.
    fn of(network: &Network) -> Self {
        Self {
            network: network.clone(),
            inspections: Inspections::default(),
            changes: Arc::default(),
            drafts: Arc::default(),
            withdrawals: Arc::default(),
        }
    }

    fn inspections(&self) -> Vec<(BlockHeight, Vec<Address>)> {
        self.inspections
            .lock()
            .expect("interpreter inspections")
            .clone()
    }

    fn reports(&self, height: BlockHeight, changes: OutputChanges) {
        self.changes
            .lock()
            .expect("interpreter changes")
            .insert(height, changes);
    }

    /// One credit of `amount` to `owner`, found in `block`.
    fn credits(&self, block: &BlockRef, owner: &Address, amount: u64) {
        let network = owner.network.clone();
        let height = block.height;
        self.drafts
            .lock()
            .expect("interpreter drafts")
            .entry(block.hash.0.clone())
            .or_default()
            .push(indexing::ObservationDraft {
                transaction: indexing::Transaction {
                    transaction_id: TransactionRef::new(
                        network.clone(),
                        format!("credit-{height:?}"),
                    ),
                    movements: vec![indexing::ValueMovement::mint(
                        indexing::MovementId("0".into()),
                        indexing::AssetId::native(network.chain.clone()),
                        base::Decimal::from(amount),
                        owner.clone(),
                    )],
                    fee: None,
                },
                transaction_order: 0,
                status: indexing::ObservationDraftStatus::Included,
            });
    }

    /// One more transaction for `block`, reported exactly as given.
    fn reports_draft(&self, block: &BlockRef, draft: indexing::ObservationDraft) {
        self.drafts
            .lock()
            .expect("interpreter drafts")
            .entry(block.hash.0.clone())
            .or_default()
            .push(draft);
    }

    /// One credit of `amount` to `owner` that `block` made with no transaction
    /// to name it.
    fn withdraws(&self, block: &BlockRef, owner: &Address, amount: u64) {
        self.withdrawals
            .lock()
            .expect("interpreter withdrawals")
            .entry(block.hash.0.clone())
            .or_default()
            .push(indexing::BlockCredit {
                id: indexing::MovementId(format!("withdrawal-{}", block.height.0)),
                asset: indexing::AssetId::native(owner.network.chain.clone()),
                amount: base::Decimal::from(amount),
                to: owner.clone(),
            });
    }

    /// The credits `block` made for the addresses this pass selected, exactly as
    /// a chain carries a withdrawal only for an address it was asked about.
    fn credited(&self, block: &BlockRef, addresses: &[Address]) -> Vec<indexing::BlockCredit> {
        let held = self.withdrawals.lock().expect("interpreter withdrawals");
        let credits = held.get(&block.hash.0).cloned().unwrap_or_default();
        credits
            .into_iter()
            .filter(|credit| addresses.contains(&credit.to))
            .collect()
    }
}

impl BlockInterpreter for Interpreter {
    type Block = TestBlock;

    fn settlement(&self) -> indexing::FeeSettlement {
        indexing::FeeSettlement::Charged
    }

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        self.inspections
            .lock()
            .expect("interpreter inspections")
            .push((block.0.height, addresses.to_vec()));
        let outputs = self
            .changes
            .lock()
            .expect("interpreter changes")
            .get(&block.0.height)
            .cloned()
            .unwrap_or_default();
        Ok(InterpretedBlock {
            outputs,
            credits: self.credited(&block.0, addresses),
            ..InterpretedBlock::new(
                &self.network,
                block.0.clone(),
                self.drafts
                    .lock()
                    .expect("interpreter drafts")
                    .get(&block.0.hash.0)
                    .cloned()
                    .unwrap_or_default(),
            )?
        })
    }
}

/// What `index` holds above `line`: the one reading of its pending blocks an
/// index hands out.
///
/// For asserting what the index holds between passes, where nothing commits
/// and `line` is whatever the scenario last saw. It is not how a read is
/// composed: a read samples first, reads the store, and narrows by the
/// checkpoint that answer came with (`indexing::PendingJoin`).
fn held(index: &dyn Pending, line: Option<&BlockRef>) -> Vec<CanonicalBlock> {
    index
        .sampled()
        .expect("a sample")
        .above(&LastSeen(line.cloned()))
}

/// The line a scenario last saw, standing in for a store's answer.
///
/// Only for [`held`]: nothing commits between the passes it is asked about, so
/// the line cannot have moved. A read has no such value to offer, and narrows
/// by the answer itself.
struct LastSeen(Option<BlockRef>);

impl indexing::Checkpointed for LastSeen {
    fn checkpoint(&self) -> Option<&BlockRef> {
        self.0.as_ref()
    }
}

/// Every amount the transactions of `blocks` move to `owner`, oldest first.
fn credited(blocks: &[CanonicalBlock], owner: &Address) -> Vec<base::Decimal> {
    blocks
        .iter()
        .flat_map(CanonicalBlock::transactions)
        .flat_map(|transaction| &transaction.transaction.movements)
        .filter(|movement| movement.to() == Some(owner))
        .map(|movement| movement.amount.clone())
        .collect()
}

/// Watches every filter of `selection` on `index`, then runs one pass.
///
/// The index owns its watch list now, so a fixture states the selection
/// through `watch` instead of handing it to the pass. Re-watching the same
/// address under the same id and start is a no-op, so a repeated pass states
/// the same selection.
fn pass(index: &dyn Indexing, selection: &[ForwardRange]) -> Result<SyncStatus, IndexError> {
    watch_all(index, selection).expect("watch");
    block_on(index.sync())
}

/// The refusal `watch` answers for a selection no index will accept.
fn watch_error(index: &dyn Watching, selection: &[ForwardRange]) -> IndexError {
    watch_all(index, selection).expect_err("an index must refuse this selection")
}

fn watch_all(index: &dyn Watching, selection: &[ForwardRange]) -> Result<(), IndexError> {
    for (position, filter) in selection.iter().enumerate() {
        block_on(index.watch(
            &format!("watch-{position}"),
            &subject(&filter.address),
            Birthday::At(filter.start_position),
        ))?;
    }
    Ok(())
}

fn subject(address: &Address) -> Subject {
    Subject {
        address: address.clone(),
        asset: indexing::AssetId::native(address.network.chain.clone()),
    }
}

fn network(chain: &str) -> Network {
    Network {
        chain: ChainName::new(chain),
        name: "testnet".into(),
    }
}

fn address(network: &Network, value: &str) -> Address {
    Address {
        network: network.clone(),
        value: value.into(),
    }
}

fn hash(height: BlockHeight) -> BlockHash {
    BlockHash(height.0.to_be_bytes().to_vec())
}

fn block(height: BlockHeight) -> BlockRef {
    BlockRef {
        position: BlockPosition(height.0),
        height,
        hash: hash(height),
        parent: height.0.checked_sub(1).map(|parent| BlockParent {
            position: BlockPosition(parent),
            hash: hash(BlockHeight(parent)),
        }),
        timestamp: None,
    }
}

fn sparse_block(
    position: u64,
    height: u64,
    hash_byte: u8,
    parent_position: u64,
    parent_hash: u8,
) -> BlockRef {
    BlockRef {
        position: BlockPosition(position),
        height: BlockHeight(height),
        hash: BlockHash(vec![hash_byte]),
        parent: Some(BlockParent {
            position: BlockPosition(parent_position),
            hash: BlockHash(vec![parent_hash]),
        }),
        timestamp: Some(1_000 + position),
    }
}

fn sparse_chain() -> Vec<BlockRef> {
    vec![
        sparse_block(97, 49, 0, 94, 9),
        sparse_block(100, 50, 1, 97, 0),
        sparse_block(103, 51, 2, 100, 1),
        sparse_block(107, 52, 3, 103, 2),
    ]
}

fn replacement_chain() -> Vec<BlockRef> {
    vec![
        sparse_block(97, 49, 0, 94, 9),
        sparse_block(100, 50, 1, 97, 0),
        sparse_block(104, 51, 12, 100, 1),
        sparse_block(108, 52, 13, 104, 12),
    ]
}

fn config(network: Network) -> SyncConfig {
    SyncConfig::new(&network, SyncPolicy::new(100)).expect("sync config")
}

fn bounded_config(network: Network, batch_size: usize) -> SyncConfig {
    SyncConfig::new(&network, SyncPolicy::new(batch_size)).expect("bounded sync config")
}

#[test]
fn source_contract_is_complete_bounded_and_position_addressed() {
    let source = Source::new(4);

    assert_eq!(block_on(source.tip()).expect("tip"), block(BlockHeight(4)));
    let blocks = block_on(source.blocks(BlockPosition(1), BlockPosition(4), 2))
        .expect("bounded inclusive range");
    assert_eq!(
        blocks
            .iter()
            .map(NativeBlock::block_ref)
            .collect::<Vec<_>>(),
        [block(BlockHeight(1)), block(BlockHeight(2))]
    );
    assert_eq!(
        block_on(source.canonical_at(BlockPosition(3))).expect("canonical lookup"),
        Some(block(BlockHeight(3)))
    );
    assert_eq!(
        block_on(source.canonical_at(BlockPosition(5))).expect("omitted position"),
        None
    );
    let error = block_on(source.blocks(BlockPosition(1), BlockPosition(4), 0))
        .expect_err("zero returned-block limit");
    assert!(!error.retryable);
}

#[test]
fn sparse_sync_uses_actual_blocks_and_resumes_a_bounded_prefix() {
    let own_network = network("sparse-chain");
    let owner = address(&own_network, "owner");
    let source = Source::sparse(sparse_chain());
    let interpreter = Interpreter::of(&own_network);
    let service = Indexer::new(
        source,
        interpreter.clone(),
        Repository::default(),
        bounded_config(own_network, 2),
    );
    let filters = vec![ForwardRange {
        address: owner.clone(),
        start_position: BlockPosition(102),
    }];

    let first = pass(&service, &filters).expect("first sparse prefix");
    assert_eq!(first.phase, SyncPhase::CatchingUp);
    assert_eq!(first.checkpoint, Some(sparse_chain()[2].clone()));

    let second = pass(&service, &filters).expect("second sparse prefix");
    assert_eq!(second.phase, SyncPhase::Ready);
    assert_eq!(second.checkpoint, Some(sparse_chain()[3].clone()));
    assert_eq!(
        interpreter.inspections(),
        [
            (BlockHeight(50), Vec::new()),
            (BlockHeight(51), vec![owner.clone()]),
            (BlockHeight(52), vec![owner]),
        ]
    );
}

#[test]
fn sparse_birthday_anchor_consumes_the_only_batch_slot_and_survives_restart() {
    let own_network = network("anchor-prefix");
    let owner = address(&own_network, "owner");
    let filters = vec![ForwardRange {
        address: owner.clone(),
        start_position: BlockPosition(102),
    }];
    let source = Source::sparse(sparse_chain());
    let interpreter = Interpreter::of(&own_network);
    let repository = Repository::default();
    let initial = Indexer::new(
        source.clone(),
        interpreter.clone(),
        repository.clone(),
        bounded_config(own_network.clone(), 1),
    );
    let status = pass(&initial, &filters).expect("anchor-only prefix");
    assert_eq!(status.phase, SyncPhase::CatchingUp);
    assert_eq!(status.checkpoint, Some(sparse_chain()[1].clone()));
    assert_eq!(source.requests(), [BlockHeight(51), BlockHeight(50)]);
    assert_eq!(interpreter.inspections(), [(BlockHeight(50), Vec::new())]);
    drop(initial);

    let restarted_source = Source::sparse(sparse_chain());
    let restarted = Indexer::new(
        restarted_source.clone(),
        interpreter.clone(),
        repository,
        bounded_config(own_network, 1),
    );
    let next = pass(&restarted, &filters).expect("first selected block after restart");
    assert_eq!(next.phase, SyncPhase::CatchingUp);
    assert_eq!(next.checkpoint, Some(sparse_chain()[2].clone()));
    let last = pass(&restarted, &filters).expect("remaining sparse block");
    assert_eq!(last.phase, SyncPhase::Ready);
    assert_eq!(last.checkpoint, Some(sparse_chain()[3].clone()));
    assert_eq!(
        restarted_source.requests(),
        [BlockHeight(51), BlockHeight(52)]
    );
    assert_eq!(
        interpreter.inspections(),
        [
            (BlockHeight(50), Vec::new()),
            (BlockHeight(51), vec![owner.clone()]),
            (BlockHeight(52), vec![owner]),
        ]
    );
}

#[test]
fn mismatched_birthday_anchor_fails_before_effects_and_allows_a_retry() {
    let own_network = network("anchor-mismatch");
    let filters = vec![ForwardRange {
        address: address(&own_network, "owner"),
        start_position: BlockPosition(102),
    }];
    let mut invalid = sparse_chain();
    invalid[2].parent.as_mut().unwrap().hash = BlockHash(vec![99]);
    let source = Source::sparse(invalid);
    let interpreter = Interpreter::of(&own_network);
    let repository = Repository::default();
    let service = Indexer::new(
        source.clone(),
        interpreter.clone(),
        repository.clone(),
        config(own_network.clone()),
    );
    let error = pass(&service, &filters).expect_err("parent hash must match the anchor");
    assert_eq!(
        error,
        IndexError::new(
            IndexErrorKind::Disconnected,
            "birthday anchor does not match the first block parent",
            true,
        )
    );
    assert_eq!(source.requests(), [BlockHeight(51), BlockHeight(50)]);
    assert!(interpreter.inspections().is_empty());
    assert_eq!(block_on(repository.checkpoint(&own_network)).unwrap(), None);

    source.replace(sparse_chain());
    let retry = pass(&service, &filters).expect("failed bootstrap releases running guard");
    assert_eq!(retry.phase, SyncPhase::Ready);
    assert_eq!(retry.checkpoint, Some(sparse_chain()[3].clone()));
}

#[test]
fn future_birthday_establishes_only_an_empty_tip_anchor() {
    let own_network = network("future-birthday");
    let source = Source::new(4);
    let interpreter = Interpreter::of(&own_network);
    let service = Indexer::new(
        source.clone(),
        interpreter.clone(),
        Repository::default(),
        config(own_network.clone()),
    );
    let status = pass(
        &service,
        &[ForwardRange {
            address: address(&own_network, "future"),
            start_position: BlockPosition(5),
        }],
    )
    .expect("future selection can establish its checkpoint");
    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(status.checkpoint, Some(block(BlockHeight(4))));
    assert_eq!(source.requests(), [BlockHeight(4)]);
    assert_eq!(interpreter.inspections(), [(BlockHeight(4), Vec::new())]);
}

#[test]
fn malformed_source_parent_is_retryable_before_interpretation_or_commit() {
    let mut genesis_with_parent = block(BlockHeight(0));
    genesis_with_parent.parent = Some(BlockParent {
        position: BlockPosition(1),
        hash: hash(BlockHeight(1)),
    });
    let mut non_genesis_without_parent = block(BlockHeight(1));
    non_genesis_without_parent.parent = None;

    for (block, message) in [
        (genesis_with_parent, "genesis block must not have a parent"),
        (
            non_genesis_without_parent,
            "non-genesis block is missing its parent",
        ),
    ] {
        let own_network = network("malformed-source");
        let interpreter = Interpreter::of(&own_network);
        let repository = Repository::default();
        let service = Indexer::new(
            Source::sparse([block]),
            interpreter.clone(),
            repository.clone(),
            config(own_network.clone()),
        );

        let error = pass(&service, &[])
            .expect_err("malformed source reference must fail before interpretation");
        assert_eq!(
            error,
            IndexError::new(IndexErrorKind::InvalidBlock, message, true)
        );
        assert!(interpreter.inspections().is_empty());
        assert_eq!(
            block_on(repository.checkpoint(&own_network)).expect("checkpoint lookup"),
            None
        );
    }
}

/// A source that answers every block request with the same blocks, whatever
/// range and limit it was asked for.
///
/// [`Source`] cannot express this: it cuts its own answer to the range and the
/// limit, so no scenario built on it can return a block the request excluded.
struct Unbounded {
    inner: Source,
    answer: Vec<BlockRef>,
}

impl ChainTips for Unbounded {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.inner.tip()
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.inner.confirmed_tip()
    }
}

impl BlockSource for Unbounded {
    type Block = TestBlock;

    fn blocks<'a>(
        &'a self,
        _start: BlockPosition,
        _end: BlockPosition,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        let answer = self.answer.iter().cloned().map(TestBlock).collect();
        Box::pin(async move { Ok(answer) })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        self.inner.canonical_at(position)
    }
}

/// What one pass answers when the source serves `answer` to every request, and
/// what it left behind: the refusal, how many blocks were interpreted, and the
/// checkpoint afterwards.
///
/// An empty index anchors at the tip and asks for position 4 alone; one
/// holding block 0 asks for positions 1 through 4, a hundred at most.
fn refused(held: Option<BlockRef>, answer: Vec<BlockRef>) -> (IndexError, usize, Option<BlockRef>) {
    let own_network = network("unbounded-source");
    let interpreter = Interpreter::of(&own_network);
    let repository = Repository::default();
    repository.state.lock().unwrap().checkpoint = held;
    let service = Indexer::new(
        Unbounded {
            inner: Source::new(4),
            answer,
        },
        interpreter.clone(),
        repository.clone(),
        config(own_network.clone()),
    );

    let error = pass(&service, &[]).expect_err("the answer must be refused");

    (
        error,
        interpreter.inspections().len(),
        block_on(repository.checkpoint(&own_network)).expect("checkpoint lookup"),
    )
}

/// An answer that is wrong in itself is an `InvalidBlock`: more blocks than
/// were asked for, a block outside the range, positions that do not ascend.
///
/// Every block here is well formed and links to its parent, so nothing but the
/// answer's own shape can be what fails. Sharing a kind with a fork in flight
/// would leave an operator reading `kind` unable to tell a moving chain from a
/// source contradicting the request it was given. It is retried all the same,
/// and it gets no further than the fetch.
#[test]
fn an_answer_that_contradicts_its_request_is_a_retryable_invalid_block() {
    let at = |height: u64| block(BlockHeight(height));
    let misplaced = "source blocks are outside the range or not strictly increasing";
    for (held, answer, message) in [
        (
            None,
            vec![at(3), at(4)],
            "source exceeded the returned-block limit",
        ),
        (None, vec![at(3)], misplaced),
        (Some(at(0)), vec![at(2), at(1)], misplaced),
        (Some(at(0)), vec![at(1), at(1)], misplaced),
    ] {
        let (error, inspected, checkpoint) = refused(held.clone(), answer);

        assert_eq!(
            error,
            IndexError::new(IndexErrorKind::InvalidBlock, message, true)
        );
        assert_eq!(inspected, 0, "{message}");
        assert_eq!(checkpoint, held, "{message}");
    }
}

/// An answer whose blocks are well formed and within the request, but do not
/// join up, is `Disconnected`: nothing where the source's own tip says a block
/// is, a block that skips the one before it, a block the source no longer
/// calls canonical. A chain that moved between two round trips explains every
/// one of them, which is what separates them from the test above.
#[test]
fn an_answer_that_does_not_join_up_is_disconnected() {
    let at = |height: u64| block(BlockHeight(height));
    let replaced = BlockRef {
        hash: BlockHash(vec![99]),
        ..at(4)
    };
    for (held, answer, message) in [
        (
            None,
            Vec::new(),
            "source did not return the required produced block",
        ),
        (
            Some(at(0)),
            Vec::new(),
            "source returned no produced block before its observed tip",
        ),
        (
            Some(at(0)),
            vec![at(2)],
            "source block does not follow the block before it",
        ),
        (None, vec![replaced], "source block changed before commit"),
    ] {
        let (error, _, checkpoint) = refused(held.clone(), answer);

        assert_eq!(
            error,
            IndexError::new(IndexErrorKind::Disconnected, message, true)
        );
        assert_eq!(checkpoint, held, "{message}");
    }
}

#[cfg(test)]
#[test]
fn checkpoint_exhaustion_follows_parent_validation_and_precedes_interpretation() {
    for parent_present in [false, true] {
        let own_network = network("exhausted-checkpoint");
        let mut checkpoint = block(BlockHeight(1));
        checkpoint.height = BlockHeight(u64::MAX);
        let mut next = block(BlockHeight(2));
        if !parent_present {
            next.parent = None;
        }
        let repository = Repository::default();
        repository.state.lock().unwrap().checkpoint = Some(checkpoint.clone());
        let interpreter = Interpreter::of(&own_network);
        let service = Indexer::new(
            Source::sparse([checkpoint.clone(), next]),
            interpreter.clone(),
            repository.clone(),
            config(own_network.clone()),
        );
        let error = pass(&service, &[]).unwrap_err();
        let expected = if parent_present {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "checkpoint height is exhausted",
                false,
            )
        } else {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "non-genesis block is missing its parent",
                true,
            )
        };
        assert_eq!(error, expected);
        assert!(interpreter.inspections().is_empty());
        assert_eq!(
            block_on(repository.checkpoint(&own_network)).unwrap(),
            Some(checkpoint),
        );
    }
}

/// The whole of a reorg shallower than the confirmed line: the durable store
/// is not touched, because the replaced blocks were never in it.
#[test]
fn a_reorg_above_the_line_changes_no_stored_byte() {
    let own_network = network("sparse-reorg");
    let owner = address(&own_network, "owner");
    // Two blocks pending: heights 51 and 52 are above the line, 50 is at it.
    let source = Source::sparse(sparse_chain()).confirming(2);
    let repository = Repository::default();
    let service = Indexer::new(
        source.clone(),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network.clone()),
    );
    let filters = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(100),
    }];
    let first = pass(&service, &filters).expect("initial sparse chain");
    assert_eq!(first.checkpoint, Some(sparse_chain()[1].clone()));
    assert_eq!(first.pending_tip, Some(sparse_chain()[3].clone()));
    let before = repository.stored();

    source.replace(replacement_chain());
    let status = pass(&service, &filters).expect("reorg above the line");

    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(status.checkpoint, Some(sparse_chain()[1].clone()));
    assert_eq!(status.pending_tip, Some(replacement_chain()[3].clone()));
    assert_eq!(repository.stored(), before);
}

/// The one failure an append-only store cannot absorb, and the only one the
/// design admits: the chain reorganized past a block already committed as
/// irreversible. Terminal, and its message names the remedy.
#[test]
fn a_reorg_below_the_line_quarantines_the_network_with_an_actionable_message() {
    let own_network = network("sparse-deep-reorg");
    let owner = address(&own_network, "owner");
    let source = Source::sparse(sparse_chain());
    let repository = Repository::default();
    let service = Indexer::new(
        source.clone(),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network),
    );
    let filters = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(100),
    }];
    pass(&service, &filters).expect("initial sparse chain");
    let before = repository.stored();

    source.replace(replacement_chain());
    let error = pass(&service, &filters).expect_err("reorg below the line");

    assert_eq!(error.kind, IndexErrorKind::ConfirmedReorg);
    assert!(!error.retryable);
    assert_eq!(
        error.message,
        "committed block at height 52 is no longer canonical; the chain \
         reorganized past its confirmed line and this network must be rescanned"
    );
    assert_eq!(repository.stored(), before);
}

/// The way back from that quarantine, end to end : the
/// offline rescan empties the network, and the next start finds no checkpoint
/// and re-indexes from the earliest registered start position — not from
/// genesis, and not from the tip.
#[test]
fn a_rescanned_network_reindexes_from_its_registered_start() {
    let own_network = network("rescan");
    let owner = address(&own_network, "owner");
    let filters = vec![ForwardRange {
        address: owner.clone(),
        start_position: BlockPosition(102),
    }];
    let source = Source::sparse(sparse_chain());
    let interpreter = Interpreter::of(&own_network);
    let repository = Repository::default();
    for block in &sparse_chain()[2..] {
        interpreter.credits(block, &owner, 5);
    }
    let quarantined = Indexer::new(
        source.clone(),
        interpreter,
        repository.clone(),
        config(own_network.clone()),
    );
    pass(&quarantined, &filters).expect("initial sparse chain");
    source.replace(replacement_chain());
    let error = pass(&quarantined, &filters).expect_err("reorg below the line");
    assert_eq!(error.kind, IndexErrorKind::ConfirmedReorg);
    drop(quarantined);

    let removed = block_on(repository.purge_network(&own_network)).expect("offline rescan");

    assert_eq!(removed.checkpoint, 1);
    assert_eq!(removed.history, 2, "both credited blocks were committed");
    assert_eq!(repository.stored(), (BTreeMap::new(), None));

    // A rescan is followed by an ordinary start: new process, same store.
    let restarted = Interpreter::of(&own_network);
    for block in &replacement_chain()[2..] {
        restarted.credits(block, &owner, 5);
    }
    let reindexing = Indexer::new(
        Source::sparse(replacement_chain()),
        restarted.clone(),
        repository.clone(),
        config(own_network),
    );
    let status = pass(&reindexing, &filters).expect("the rescanned network re-indexes");

    assert_eq!(status.phase, SyncPhase::Ready);
    assert_eq!(status.checkpoint, Some(replacement_chain()[3].clone()));
    assert_eq!(
        restarted.inspections(),
        [
            (BlockHeight(50), Vec::new()),
            (BlockHeight(51), vec![owner.clone()]),
            (BlockHeight(52), vec![owner]),
        ],
        "re-indexing starts at the birthday anchor, not at genesis or the tip"
    );
    let (blocks, checkpoint) = repository.stored();
    assert_eq!(checkpoint, Some(replacement_chain()[3].clone()));
    assert_eq!(
        blocks.keys().copied().collect::<Vec<_>>(),
        [BlockHeight(50), BlockHeight(51), BlockHeight(52)]
    );
}

/// A chain that states a line it cannot keep is refused rather than believed:
/// committing a block above the source's own tip is exactly what this design
/// exists to prevent.
#[test]
fn a_confirmed_tip_above_the_observed_tip_is_a_retryable_source_failure() {
    let own_network = network("lying-source");
    let source = Ahead(Source::new(4));
    let service = Indexer::new(
        source,
        Interpreter::of(&own_network),
        Repository::default(),
        config(own_network),
    );

    let error = pass(&service, &[]).expect_err("impossible line");

    assert_eq!(
        error,
        IndexError::new(
            IndexErrorKind::Source,
            "source reports a confirmed tip above its own tip",
            true,
        )
    );
}

/// A source whose chain advances between the two lines one pass reads, which
/// is every source: `tip` and `confirmed_tip` are two round trips, and a chain
/// does not stand still between them.
///
/// The advance happens after whichever of the two is read first, so the case
/// is about the *order* the pass reads them in and nothing else.
struct Advancing {
    inner: Source,
    advance: Mutex<Option<Vec<BlockRef>>>,
    reads: AtomicUsize,
}

impl Advancing {
    /// `inner`, becoming `advance` the moment its first line has been read.
    fn new(inner: Source, advance: Vec<BlockRef>) -> Self {
        Self {
            inner,
            advance: Mutex::new(Some(advance)),
            reads: AtomicUsize::new(0),
        }
    }

    fn advanced(&self) {
        if self.reads.fetch_add(1, Ordering::AcqRel) == 0
            && let Some(blocks) = self.advance.lock().expect("advance").take()
        {
            self.inner.replace(blocks);
        }
    }
}

impl ChainTips for Advancing {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        let read = self.inner.tip();
        Box::pin(async move {
            let observed = read.await;
            self.advanced();
            observed
        })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        let read = self.inner.confirmed_tip();
        Box::pin(async move {
            let confirmed = read.await;
            self.advanced();
            confirmed
        })
    }
}

impl BlockSource for Advancing {
    type Block = TestBlock;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        self.inner.blocks(start, end, limit)
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        self.inner.canonical_at(position)
    }
}

/// The other side of the guard above: a chain that merely *moved* between the
/// two round trips is not a lying source.
///
/// The guard compares two values read at two different moments, so which one
/// is read first decides whether an honest chain can fail it. Reading the tip
/// first leaves the *older* number as the ceiling: let more than
/// `confirmation_depth` blocks arrive in the gap — two Bitcoin blocks found
/// close together, a handful of Solana slots — and a source that never said
/// anything untrue is accused of stating a line above its own tip. Reading the
/// confirmed line first makes the ceiling the *newer* number, so an advance
/// can only widen the window, never invert it.
///
/// It cost readiness, not correctness: the failure is retryable, so the next
/// pass succeeded, but the network published `Retrying` in between and the
/// process answered 503 on `/health/ready` for a chain that was perfectly
/// healthy.
#[test]
fn a_chain_that_advances_between_the_two_lines_is_not_a_lying_source() {
    let own_network = network("advancing-source");
    let source = Advancing::new(
        Source::new(0).confirming(1),
        (0..=2).map(|height| block(BlockHeight(height))).collect(),
    );
    let service = Indexer::new(
        source,
        Interpreter::of(&own_network),
        Repository::default(),
        config(own_network),
    );

    let statuses = pass(&service, &[]).expect("a chain that moved forward is not a liar");

    assert_eq!(
        statuses.checkpoint,
        Some(block(BlockHeight(0))),
        "the line the source had confirmed at the moment it was read; what \
         arrived afterwards belongs to the next pass"
    );
    assert_eq!(
        statuses.observed_tip,
        Some(block(BlockHeight(2))),
        "and it indexed towards the tip the advance produced, so the advance \
         really did land between the two reads"
    );
}

/// A restart has no pending state, and rebuilding it is the ordinary catch-up
/// pass: the blocks above the line are fetched once, exactly as they would be
/// on a first run, and no extra call is made for them.
#[test]
fn a_restart_rebuilds_pending_with_the_ordinary_catch_up_and_no_extra_fetch() {
    let own_network = network("restart-pending");
    let repository = Repository::default();
    let filters = Vec::<ForwardRange>::new();
    let initial = Indexer::new(
        Source::new(4).confirming(2),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network.clone()),
    );
    let status = pass(&initial, &filters).expect("initial sync");
    assert_eq!(status.checkpoint, Some(block(BlockHeight(2))));
    assert_eq!(status.pending_tip, Some(block(BlockHeight(4))));
    drop(initial);

    let restarted_source = Source::new(4).confirming(2);
    let restarted = Indexer::new(
        restarted_source.clone(),
        Interpreter::of(&own_network),
        repository,
        config(own_network),
    );
    let status = pass(&restarted, &filters).expect("restart sync");

    assert_eq!(status.checkpoint, Some(block(BlockHeight(2))));
    assert_eq!(status.pending_tip, Some(block(BlockHeight(4))));
    assert_eq!(
        restarted_source.requests(),
        [BlockHeight(3), BlockHeight(4)],
        "only the two blocks above the line are read again"
    );
}

/// Reading the pending blocks is not free of source failures: an outage
/// leaves the committed checkpoint alone and the next pass repairs it.
#[test]
fn a_canonical_lookup_outage_preserves_the_checkpoint_and_allows_a_retry() {
    let own_network = network("lookup-outage");
    let source = Source::sparse(sparse_chain()).confirming(2);
    let repository = Repository::default();
    let service = Indexer::new(
        source.clone(),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network.clone()),
    );
    let filters = vec![ForwardRange {
        address: address(&own_network, "owner"),
        start_position: BlockPosition(100),
    }];
    pass(&service, &filters).unwrap();
    *source.canonical_failure.lock().unwrap() = Some(BlockPosition(107));

    let error = pass(&service, &filters).unwrap_err();
    assert_eq!(
        error,
        IndexError::new(IndexErrorKind::Source, "canonical lookup unavailable", true)
    );
    assert_eq!(
        block_on(repository.checkpoint(&own_network)).unwrap(),
        Some(sparse_chain()[1].clone())
    );

    *source.canonical_failure.lock().unwrap() = None;
    let retry = pass(&service, &filters).unwrap();
    assert_eq!(retry.phase, SyncPhase::Ready);
    assert_eq!(retry.pending_tip, Some(sparse_chain()[3].clone()));
}

/// A re-inspection is published whole or not at all.
///
/// Adding a watch makes every held block's facts stale, so the pass interprets
/// all of them again. That rebuild is a sequence of awaits — a cache lookup or a
/// full refetch, plus one `canonical_at` per block — and it used to run by
/// *emptying the shared collection first* and re-admitting into it one block at a
/// time. Every read landing in that window saw a prefix of the blocks above the
/// line: a pending transaction missing from a history page, a pending credit
/// missing from a balance, and an output a pending block had spent offered as
/// spendable again. A rebuild that failed part-way left that prefix published
/// until some later pass happened to repair it, which is the part that turns a
/// window into a wrong answer with no end.
///
/// So the failure is forced here, at the second of two held blocks, and what is
/// asserted is that the index still answers with *both* of them. The rebuilt
/// collection is reachable by no reader until it is complete.
#[test]
fn a_failed_reinspection_leaves_every_pending_block_standing() {
    let own_network = network("atomic-reinspection");
    let owner = address(&own_network, "owner");
    let late = address(&own_network, "late");
    let interpreter = Interpreter::of(&own_network);
    interpreter.credits(&block(BlockHeight(3)), &owner, 5);
    interpreter.credits(&block(BlockHeight(4)), &owner, 7);
    let source = Source::new(4).confirming(2);
    let service = Arc::new(Indexer::new(
        source.clone(),
        interpreter,
        Repository::default(),
        config(own_network.clone()),
    ));
    let watched = vec![ForwardRange {
        address: owner.clone(),
        start_position: BlockPosition(0),
    }];
    let pending = || {
        let line = block_on(service.checkpoint(&own_network)).expect("the confirmed line");
        let held = held(&*service, line.as_ref());
        (held.len(), credited(&held, &owner))
    };
    let both = (
        2,
        vec![base::Decimal::from(5_u64), base::Decimal::from(7_u64)],
    );

    pass(&*service, &watched).expect("first pass");
    assert_eq!(
        pending(),
        both,
        "heights 3 and 4 are above the confirmed line and both pending"
    );

    // Watching a second address stales every held block, and the refreshed
    // interpretation of the *newer* one cannot be finished.
    *source.canonical_failure.lock().unwrap() = Some(BlockPosition(4));
    let mut selection = watched.clone();
    selection.push(ForwardRange {
        address: late,
        start_position: BlockPosition(3),
    });
    let error = pass(&*service, &selection).expect_err("the rebuild cannot finish");
    assert!(error.retryable, "a source that cannot answer is retried");

    assert_eq!(
        pending(),
        both,
        "both pending blocks are still held, at the selection it was last \
         wholly interpreted against"
    );
}

/// An observation joining the front of the chain re-interprets what is already
/// held there.
///
/// Catch-up ends by *changing the selection*: while an observation is reading its
/// own past it is not inspected for the blocks arriving now, and the moment its
/// progress reaches the line it is
/// (`indexing::WatchProgress::caught_up`). The blocks above that line were
/// already fetched and interpreted — without it — and nothing ever revisits a
/// committed block, so committing them unchanged omits the address forever while
/// its own progress advances straight through them. That is loss, not latency:
/// the claim says inspected and no block was.
///
/// The address selection is one number stamped on every held block, and it used
/// to count *publications* only. No publication happens when a catch-up
/// finishes, so the count did not move, so the held blocks read as current.
#[test]
fn an_observation_joining_the_front_reinterprets_the_blocks_above_the_line() {
    let own_network = network("catch-up-join");
    let late = address(&own_network, "late");
    let interpreter = Interpreter::of(&own_network);
    let source = Source::new(4).confirming(2);
    let repository = Repository::default();
    let service = Indexer::new(
        source.clone(),
        interpreter.clone(),
        repository.clone(),
        config(own_network.clone()),
    );

    // A checkpoint at 2 with heights 3 and 4 held above it, and nothing watched.
    let status = pass(&service, &[]).expect("the first pass");
    assert_eq!(status.checkpoint, Some(block(BlockHeight(2))));
    assert_eq!(status.pending_tip, Some(block(BlockHeight(4))));

    // An observation from the very beginning: below the line, so it is inspected
    // for what it owes and not for what is arriving.
    block_on(service.watch("late", &subject(&late), Birthday::At(BlockPosition(0))))
        .expect("an observation below the line");
    block_on(service.sync()).expect("the pass that fills its range in");
    assert_eq!(
        repository.progress("late"),
        Some(block(BlockHeight(2))),
        "its catch-up reached the line, so the next pass inspects it at the front",
    );

    // The chain advances, so height 3 is now confirmed and commits. The
    // observation is caught up to the line and joins the selection this pass
    // captures — which the blocks already held above it were not interpreted
    // against.
    source.replace((0..=5).map(|height| block(BlockHeight(height))));
    block_on(service.sync()).expect("the pass that commits height 3");
    assert_eq!(
        repository.progress("late"),
        Some(block(BlockHeight(3))),
        "its progress advanced through height 3",
    );

    let inspected_for_late = interpreter
        .inspections()
        .into_iter()
        .any(|(height, addresses)| height == BlockHeight(3) && addresses.contains(&late));
    assert!(
        inspected_for_late,
        "height 3 committed without ever being inspected for the address whose \
         progress now claims it",
    );
}

/// Both halves of the owner's question, across a reorg: what is settled, and
/// what is on its way. The confirmed rows never move; the pending blocks follow
/// the chain.
///
/// Each half is read from its owner — the settled rows from the repository, the
/// blocks above the line from the index — and the sample is narrowed by the
/// checkpoint the repository's own answer came with.
#[test]
fn confirmed_and_pending_value_are_both_correct_across_a_reorg() {
    let own_network = network("both-views");
    let owner = address(&own_network, "owner");
    let interpreter = Interpreter::of(&own_network);
    interpreter.credits(&block(BlockHeight(1)), &owner, 5);
    interpreter.credits(&block(BlockHeight(4)), &owner, 7);
    let source = Source::new(4).confirming(2);
    let repository = Repository::default();
    let service = Arc::new(Indexer::new(
        source.clone(),
        interpreter,
        repository.clone(),
        config(own_network.clone()),
    ));
    let filters = vec![ForwardRange {
        address: owner.clone(),
        start_position: BlockPosition(0),
    }];
    let subject = subject(&owner);
    let read = || {
        let sample = service.sampled().expect("a sample");
        let query = HistoryQuery::new(&subject, indexing::Page::first(10));
        let page = block_on(repository.history(query)).expect("history");
        let balance = block_on(indexing::BalanceStore::balance(
            &repository,
            indexing::BalanceQuery::new(&subject),
        ))
        .expect("balance");
        assert_eq!(page.checkpoint, balance.observed_at, "one line, both reads");
        let above = sample.above(&page);
        (page, balance, above)
    };

    pass(&*service, &filters).expect("first pass");
    let (page, balance, above) = read();
    assert_eq!(page.checkpoint, Some(block(BlockHeight(2))));
    assert_eq!(page.transactions.len(), 1, "the credit at height 1 settled");
    assert_eq!(balance.amount, base::Decimal::from(5_u64));
    assert_eq!(
        credited(&above, &owner),
        [base::Decimal::from(7_u64)],
        "the credit at height 4 has not"
    );
    assert_eq!(
        above.last().map(CanonicalBlock::block),
        Some(&block(BlockHeight(4)))
    );
    let settled = repository.stored();

    // Heights 3 and 4 are replaced by a chain that credits nothing.
    let mut replacement = (0..=2)
        .map(|height| block(BlockHeight(height)))
        .collect::<Vec<_>>();
    for height in 3..=4 {
        let mut fork = block(BlockHeight(height));
        fork.hash = BlockHash(vec![0xff, height as u8]);
        fork.parent = replacement.last().map(|parent| BlockParent {
            position: parent.position,
            hash: parent.hash.clone(),
        });
        replacement.push(fork);
    }
    let tip = replacement.last().cloned();
    source.replace(replacement);

    pass(&*service, &filters).expect("second pass");
    let (page, balance, above) = read();
    assert_eq!(repository.stored(), settled, "nothing durable moved");
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(
        balance.amount,
        base::Decimal::from(5_u64),
        "settled is settled"
    );
    assert!(
        credited(&above, &owner).is_empty(),
        "the pending credit is gone"
    );
    assert_eq!(
        above.last().map(CanonicalBlock::block),
        tip.as_ref(),
        "the pending blocks track the replacement chain"
    );
}

/// Value a block moved with no transaction to name it: held above the confirmed
/// line, settled below it, and a transaction nowhere.
///
/// The whole path for an `indexing::BlockCredit` — interpreter, pending blocks,
/// commit, store fold — which nothing else drives end to end. Each link has its
/// own test (`wallets`' pending fold, `balance.rs`'s ledger,
/// `contracts::BalanceContract::folds_a_credit_no_transaction_names`,
/// `BackfillContract::fills_in_a_credit_no_transaction_names`), and what a chain
/// actually has is an Ethereum withdrawal arriving twelve blocks above the line
/// — which is the only reason the pending half of this record exists at all.
///
/// The store is `indexing::memory::Repository` rather than the `Repository` above,
/// and deliberately: that one folds history rows alone, which is exactly the
/// shape a credit is invisible to, so it could not tell the two halves apart.
#[test]
fn a_transactionless_credit_is_pending_above_the_line_and_then_settles() {
    let own_network = network("withdrawals");
    let beneficiary = address(&own_network, "beneficiary");
    let interpreter = Interpreter::of(&own_network);
    interpreter.withdraws(&block(BlockHeight(1)), &beneficiary, 5);
    interpreter.withdraws(&block(BlockHeight(4)), &beneficiary, 7);
    let source = Source::new(4).confirming(2);
    let memory = memory::Repository::new();
    let service = Arc::new(Indexer::new(
        source.clone(),
        interpreter,
        memory.clone(),
        config(own_network.clone()),
    ));
    let of = subject(&beneficiary);
    block_on(service.watch("beneficiary", &of, Birthday::At(BlockPosition(0))))
        .expect("an observation from the beginning");
    let read = || {
        let sample = service.sampled().expect("a sample");
        let balance = block_on(indexing::BalanceStore::balance(
            &memory,
            indexing::BalanceQuery::new(&of),
        ))
        .expect("balance");
        let above = sample.above(&balance);
        let arriving = above
            .iter()
            .flat_map(CanonicalBlock::credits)
            .filter(|credit| credit.to == beneficiary)
            .map(|credit| credit.amount.clone())
            .collect::<Vec<_>>();
        (balance, above, arriving)
    };

    block_on(Indexing::sync(&*service)).expect("the first pass");

    let (balance, above, arriving) = read();
    assert_eq!(balance.observed_at, Some(block(BlockHeight(2))));
    assert_eq!(
        balance.amount,
        base::Decimal::from(5_u64),
        "the credit at height 1 settled, out of a collection no transaction \
         reaches"
    );
    assert_eq!(
        arriving,
        [base::Decimal::from(7_u64)],
        "and the one at height 4 is held as on its way rather than withheld \
         for the whole depth of the line"
    );
    let query = HistoryQuery::new(&of, indexing::Page::first(10));
    let page = block_on(memory.history(query)).expect("history");
    assert!(
        page.transactions.is_empty() && above.iter().all(|held| held.transactions().is_empty()),
        "a credit is not a transaction and no chain can express one as the \
         other: an address with a balance and an empty history is the shape the \
         record exists for"
    );

    // The chain advances by two, so height 4 falls below the line.
    source.replace((0..=6).map(|height| block(BlockHeight(height))));
    block_on(Indexing::sync(&*service)).expect("the pass that confirms height 4");

    let (balance, _, arriving) = read();
    assert_eq!(
        balance.amount,
        base::Decimal::from(12_u64),
        "the settled total gained exactly the credit that was pending"
    );
    assert!(arriving.is_empty());
}

/// A UTXO chain spends its own change at depth one: the durable table holds
/// confirmed outputs and the index holds the rest, as facts a consumer folds
/// over the store's page (`indexing::PendingJoin`).
#[test]
fn an_output_created_above_the_line_is_held_for_selection_at_depth_one() {
    let own_network = network("change-at-depth-one");
    let owner = address(&own_network, "owner");
    let change = IndexedOutput {
        id: OutputId {
            transaction: TransactionRef::new(own_network.clone(), "change"),
            index: 0,
        },
        address: owner.clone(),
        asset: indexing::AssetId::native(own_network.chain.clone()),
        amount: base::Decimal::from(9_u64),
        evidence: vec![7],
        created_at: BlockHeight(4),
        coinbase: false,
    };
    let interpreter = Interpreter::of(&own_network);
    interpreter.reports(
        BlockHeight(4),
        OutputChanges {
            created: vec![change.clone()],
            ..OutputChanges::default()
        },
    );
    let repository = Repository::default();
    let service = Indexer::new(
        Source::new(4).confirming(2),
        interpreter,
        repository.clone(),
        config(own_network),
    );
    let filters = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(0),
    }];

    let status = pass(&service, &filters).expect("pass");

    assert_eq!(status.checkpoint, Some(block(BlockHeight(2))));
    let sample = service.sampled().expect("a sample");
    let page = block_on(indexing::OutputStore::list(
        &repository,
        indexing::OutputRequest {
            network: change.address.network.clone(),
            address: change.address.clone(),
            after: None,
            limit: 10,
        },
    ))
    .expect("the confirmed outputs");
    assert!(
        page.outputs.is_empty(),
        "the durable table holds confirmed outputs only"
    );
    let created = sample
        .above(&page)
        .iter()
        .flat_map(|held| held.outputs().created.clone())
        .map(|output| output.id)
        .collect::<Vec<_>>();
    assert_eq!(
        created,
        [change.id],
        "the pending blocks carry the change, so it is spendable at depth one"
    );
}

/// An output is spendable only once the index can prove it was not spent, and a
/// partly inspected history cannot.
///
/// The read is two facts: rows, and the checkpoint they were read at. Under a
/// bounded batch a catch-up inspects the bottom of a range first, so the store
/// legitimately holds an output created at height 0 while the spend at height 1
/// has not been looked at — and the page still comes back stamped with the
/// network's real checkpoint, far above both. Every row in it is unspent *as far
/// as has been read*, which is not what the page says.
///
/// This one cannot be weakened the way a balance or a history page is, because
/// the consumer is coin selection: it pages the set and spends the union, so a
/// row offered here becomes a transaction input. The page is refused until the
/// inspection reaches the checkpoint it would be stamped with.
///
/// The refusal is the repository's, made from the progress the passes recorded
/// in it; what this drives end to end is that the index records a partial
/// catch-up as exactly that. Both shipped stores are held to the same refusal by
/// `BackfillContract::offers_no_output_until_the_range_is_read`.
#[test]
fn a_partly_inspected_history_offers_no_spendable_output() {
    let own_network = network("partial-catch-up-outputs");
    let owner = address(&own_network, "late");
    let interpreter = Interpreter::of(&own_network);
    let repository = Repository::default();
    let service = Indexer::new(
        Source::new(8),
        interpreter.clone(),
        repository.clone(),
        // One block of catch-up per pass, so exactly the creating block is read.
        bounded_config(own_network.clone(), 1),
    );
    let funding = IndexedOutput {
        id: OutputId {
            transaction: TransactionRef::new(own_network.clone(), "old"),
            index: 0,
        },
        address: owner.clone(),
        asset: indexing::AssetId::native(own_network.chain.clone()),
        amount: base::Decimal::from(9_u64),
        evidence: vec![7],
        created_at: BlockHeight(0),
        coinbase: false,
    };
    interpreter.reports(
        BlockHeight(0),
        OutputChanges {
            created: vec![funding.clone()],
            ..OutputChanges::default()
        },
    );
    interpreter.reports(
        BlockHeight(1),
        OutputChanges {
            spent: vec![funding.key()],
            ..OutputChanges::default()
        },
    );

    block_on(service.sync()).expect("the pass that anchors the checkpoint");
    block_on(service.watch("late", &subject(&owner), Birthday::At(BlockPosition(0))))
        .expect("an observation from the beginning");
    let status = block_on(service.sync()).expect("the pass that reads one owed block");
    assert_eq!(status.checkpoint, Some(block(BlockHeight(8))));
    assert!(
        status.backfill.is_some(),
        "the range below the line is not read out yet",
    );
    assert!(
        repository.outputs().contains(&funding.key()),
        "the store legitimately holds the output height 0 created",
    );

    let error = block_on(indexing::OutputStore::list(
        &repository,
        indexing::OutputRequest {
            network: own_network,
            address: owner,
            after: None,
            limit: 100,
        },
    ))
    .expect_err("a spendable set nothing can prove must not be offered");

    assert_eq!(error.kind, IndexErrorKind::Conflict);
    // Named, not just classified: a store raises the same retryable `Conflict`
    // when the checkpoint moves under a cursor, and this scenario passes no
    // cursor at all — so the message is what says which refusal this is.
    assert!(
        error
            .message
            .contains("not readable while this address's own history"),
        "the inspection refused it, not the cursor: {}",
        error.message
    );
    assert!(
        error.retryable,
        "the refusal clears as the catch-up reaches the line",
    );
}

/// A source whose confirmed line walks backwards after the first pass, the
/// way a lagging replica behind a load balancer does.
struct Retreating {
    inner: Source,
    passes: AtomicUsize,
}

impl ChainTips for Retreating {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.inner.tip()
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        let retreat = self.passes.fetch_add(1, Ordering::AcqRel) > 0;
        Box::pin(async move {
            let tip = self.inner.tip().await?;
            if retreat {
                return Ok(block(BlockHeight(tip.height.0 - 2)));
            }
            Ok(tip)
        })
    }
}

impl BlockSource for Retreating {
    type Block = TestBlock;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        self.inner.blocks(start, end, limit)
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        self.inner.canonical_at(position)
    }
}

/// `ChainTips::confirmed_tip` states the line never moves backwards, and a
/// source that moves it anyway is refused rather than believed: everything the
/// store already committed above the retreated line was promised irreversible,
/// and believing the retreat would let the next pass commit above a line the
/// chain may still take back.
#[test]
fn a_confirmed_tip_that_moves_backwards_is_a_retryable_source_failure() {
    let own_network = network("retreating-source");
    let service = Indexer::new(
        Retreating {
            inner: Source::new(4),
            passes: AtomicUsize::new(0),
        },
        Interpreter::of(&own_network),
        Repository::default(),
        config(own_network),
    );

    let first = pass(&service, &[]).expect("the first line is believed");
    assert_eq!(first.checkpoint, Some(block(BlockHeight(4))));

    let error = pass(&service, &[]).expect_err("a line that walked backwards");

    assert_eq!(
        error,
        IndexError::new(
            IndexErrorKind::Source,
            "source moved its confirmed tip backwards",
            true,
        )
    );
}

/// A source whose confirmed line is one block above its own tip.
struct Ahead(Source);

impl ChainTips for Ahead {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.0.tip()
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let tip = self.0.tip().await?;
            Ok(BlockRef {
                position: BlockPosition(tip.position.0 + 1),
                height: BlockHeight(tip.height.0 + 1),
                ..tip
            })
        })
    }
}

impl BlockSource for Ahead {
    type Block = TestBlock;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        self.0.blocks(start, end, limit)
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        self.0.canonical_at(position)
    }
}

#[test]
fn fresh_wallet_sync_starts_at_its_earliest_birthday_not_genesis() {
    let own_network = network("any-chain");
    let first = address(&own_network, "first");
    let second = address(&own_network, "second");
    let interpreter = Interpreter::of(&own_network);
    let source = Source::new(4);
    let service = Indexer::new(
        source.clone(),
        interpreter.clone(),
        Repository::default(),
        config(own_network),
    );

    let statuses = pass(
        &service,
        &[
            ForwardRange {
                address: first.clone(),
                start_position: BlockPosition(3),
            },
            ForwardRange {
                address: second.clone(),
                start_position: BlockPosition(4),
            },
        ],
    )
    .expect("sync selected addresses");

    assert_eq!(statuses.phase, SyncPhase::Ready);
    assert_eq!(statuses.checkpoint, Some(block(BlockHeight(4))));
    assert_eq!(
        source.requests(),
        vec![BlockHeight(3), BlockHeight(2), BlockHeight(4)]
    );
    assert_eq!(
        interpreter.inspections(),
        vec![
            (BlockHeight(2), Vec::new()),
            (BlockHeight(3), vec![first.clone()]),
            (BlockHeight(4), vec![first, second]),
        ]
    );
}

#[test]
fn fresh_index_without_wallets_anchors_directly_at_the_tip() {
    let own_network = network("any-chain");
    let interpreter = Interpreter::of(&own_network);
    let source = Source::new(500);
    let service = Indexer::new(
        source.clone(),
        interpreter.clone(),
        Repository::default(),
        config(own_network),
    );

    let statuses = pass(&service, &[]).expect("sync without wallets");

    assert_eq!(statuses.phase, SyncPhase::Ready);
    assert_eq!(statuses.checkpoint, Some(block(BlockHeight(500))));
    assert_eq!(source.requests(), vec![BlockHeight(500)]);
    assert_eq!(
        interpreter.inspections(),
        vec![(BlockHeight(500), Vec::new())]
    );
}

#[test]
fn caller_can_restore_the_same_filter_selection_after_restart() {
    let own_network = network("any-chain");
    let owner = address(&own_network, "owner");
    let repository = Repository::default();
    let initial = Indexer::new(
        Source::new(4),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network.clone()),
    );
    let filters = vec![ForwardRange {
        address: owner.clone(),
        start_position: BlockPosition(3),
    }];
    pass(&initial, &filters).expect("initial sync");
    drop(initial);

    let restarted_source = Source::new(4);
    let restarted = Indexer::new(
        restarted_source.clone(),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network.clone()),
    );
    pass(&restarted, &filters).expect("restart sync");
    // The new process has no registry, so it does not know it already
    // observed this address — but the store does. The first pass anchored
    // itself at the earliest birthday, so nothing below the checkpoint was
    // ever missed for this watch, and it recorded exactly that. The caller
    // re-states `start_position: 3`, the re-stated watch takes over the
    // recorded answer, and no block is read again.
    assert!(
        restarted_source.requests().is_empty(),
        "a watch the store can already prove was covered is never swept"
    );

    let settled = Source::new(4);
    let again = Indexer::new(
        settled.clone(),
        Interpreter::of(&own_network),
        repository,
        config(own_network),
    );
    pass(&again, &filters).expect("settled sync");
    assert!(
        settled.requests().is_empty(),
        "recorded coverage stops the sweep from ever happening twice"
    );
}

#[test]
fn watch_rejects_an_address_from_another_network() {
    let own_network = network("own-chain");
    let service = Indexer::new(
        Source::new(0),
        Interpreter::of(&own_network),
        Repository::default(),
        config(own_network),
    );

    let error = watch_error(
        &service,
        &[ForwardRange {
            address: address(&network("other-chain"), "owner"),
            start_position: BlockPosition(0),
        }],
    );

    assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
}

/// A blank or repeated address is refused where it is offered, so no pass ever
/// carries one and no caller needs a reachable node to be told.
///
/// This and `watch_rejects_an_address_from_another_network` are now the *only*
/// enforcement of the watch selection's invariants: `SyncConfig::validate_filters`
/// re-ran the same three checks on every pass, twice per pass, and is gone
/// (`docs/findings.md`, `policy.rs:148`). The pass may read the selection
/// unchecked because nothing can put a bad address in it — a caller's request is
/// refused here, and a restart's stored watch rows are refused by
/// `WatchStore::add`, which `contracts::WatchContract::{
/// refuses_a_blank_address, refuses_a_batch_of_two_networks,
/// answers_only_its_own_network}` pin. Uniqueness needs no
/// check at all: the hot set is keyed by address, so the duplicate case below is
/// a `Conflict` from the collection's own shape.
#[test]
fn watch_rejects_empty_and_duplicate_addresses_before_source_io() {
    let own_network = network("own-chain");
    let duplicate = address(&own_network, "duplicate");
    let cases = [
        vec![ForwardRange {
            address: address(&own_network, ""),
            start_position: BlockPosition(0),
        }],
        vec![
            ForwardRange {
                address: duplicate.clone(),
                start_position: BlockPosition(1),
            },
            ForwardRange {
                address: duplicate,
                start_position: BlockPosition(2),
            },
        ],
    ];

    for filters in cases {
        let source = Source::new(10);
        let service = Indexer::new(
            source.clone(),
            Interpreter::of(&own_network),
            Repository::default(),
            config(own_network.clone()),
        );

        let error = watch_error(&service, &filters);

        assert!(matches!(
            error.kind,
            IndexErrorKind::InvalidRequest | IndexErrorKind::Conflict
        ));
        assert_eq!(source.tip_calls(), 0);
        assert!(source.requests().is_empty());
        assert!(
            block_on(service.watched(&own_network)).expect("rows").len() < filters.len(),
            "a refused filter leaves no row behind for a pass to read"
        );
    }
}

/// A registration whose caller gave up is reconciled from the store, not assumed
/// rolled back.
///
/// `WatchStore::add` says so itself: "a storage error or a cancelled future can
/// still leave the outcome uncertain — retry rather than assuming rollback". A
/// store that has written the row and not yet answered is exactly that window,
/// and a cancelled future runs no code after the await — so the durable list held
/// an observation the running index had never seen, its publication permit was
/// released as though nothing had happened, and no later pass, read or restart
/// short of a process restart would ever look again. Every block arriving for
/// that address was missed, silently and permanently.
#[test]
fn a_cancelled_registration_is_reconciled_without_a_restart() {
    let own_network = network("lost-publication");
    let repository = Repository::default();
    let service = Indexer::new(
        Source::new(8),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network.clone()),
    );
    block_on(service.sync()).expect("the pass that anchors the checkpoint");
    let owner = subject(&address(&own_network, "late"));
    repository.pause_add();

    let mut registration = service.watch("late", &owner, Birthday::At(BlockPosition(0)));
    assert!(
        registration
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending(),
        "the store has not acknowledged the row yet",
    );
    assert!(
        repository.holds("late"),
        "it is durable all the same, which is the whole difficulty",
    );
    drop(registration);

    block_on(service.sync()).expect("a pass after the cancellation");

    assert!(
        block_on(service.watched(&own_network))
            .expect("the watch list")
            .iter()
            .any(|watch| watch.id == "late"),
        "a durably registered observation must not stay invisible to the index \
         that is running",
    );
}

/// An address registered while the tip is being observed is still inspected for
/// in the blocks its birthday covers.
///
/// A wallet is anchored at the checkpoint that was current when it was created,
/// which promises every later block is inspected for it. If the pass indexed
/// against a selection read before it observed the tip, the blocks that tip
/// admits would be applied without the new address — and nothing rescans them
/// once the checkpoint moves past, so the miss is permanent rather than late.
#[test]
fn an_address_watched_while_the_tip_is_read_still_covers_its_birthday() {
    let own_network = network("late-chain");
    let late = address(&own_network, "late");
    let watched: Arc<OnceLock<Arc<Indexer<Source, Interpreter, Repository>>>> =
        Arc::new(OnceLock::new());
    let source = Source::new(2).during_tip({
        let watched = watched.clone();
        let late = late.clone();
        move || {
            let index = watched.get().expect("the service is built before it syncs");
            // The pass is mid-`tip()`, so this cannot await the pass; the
            // fixture's store and admission are both ready, so one poll
            // settles it, and a pending poll would be the defect itself.
            let late = subject(&late);
            let mut watching = Box::pin(index.watch("late", &late, Birthday::At(BlockPosition(1))));
            match watching
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                Poll::Ready(result) => drop(result.expect("watch during tip")),
                Poll::Pending => {
                    panic!("watching must not wait on the pass that is reading the tip")
                }
            }
        }
    });
    let interpreter = Interpreter::of(&own_network);
    let service = Arc::new(Indexer::new(
        source,
        interpreter.clone(),
        Repository::default(),
        config(own_network),
    ));
    watched.set(service.clone()).ok().expect("one service");

    block_on(service.sync()).expect("sync");

    let covered = interpreter
        .inspections()
        .into_iter()
        .filter(|(height, _)| *height >= BlockHeight(1))
        .collect::<Vec<_>>();
    assert!(
        !covered.is_empty(),
        "blocks at or above the address birthday must be indexed"
    );
    for (height, addresses) in covered {
        assert!(
            addresses.contains(&late),
            "block {height:?} was indexed without the address its birthday covers"
        );
    }
}

#[derive(Default)]
struct Observations(AtomicUsize);

impl indexing::Listener for Observations {
    fn committed(&self, _: &indexing::BlockCommit) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// **What a chain crate hands back is this, and nothing wrapping it.**
///
/// A `Indexer` over a chain's own source and interpreter answers the whole
/// `Indexing` surface — one network, one status, a listener list, the
/// checkpoint — and the pending sample a consumer composes its reads from, so
/// the three chain newtypes that forwarded each of those to a `Indexer` in
/// field `0`, and the macro that wrote the forwarding, were deleted with nothing
/// to replace them.
#[test]
fn the_service_alone_is_a_complete_indexer() {
    let own_network = network("unwrapped");
    let owner = address(&own_network, "owner");
    let indexer = Arc::new(Indexer::new(
        Source::new(2),
        Interpreter::of(&own_network),
        Repository::default(),
        config(own_network.clone()),
    ));
    let observations = Arc::new(Observations::default());
    indexing::Indexing::listen(&*indexer, observations.clone());
    assert_eq!(indexer.network(), &own_network);
    let filters = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(0),
    }];

    let pending: Arc<dyn Pending> = indexer.clone();
    let indexer: &dyn Indexing = &*indexer;
    let status = pass(indexer, &filters).expect("a pass through the trait object");

    assert_eq!(indexer.network(), &own_network);
    assert_eq!(status.network, own_network);
    assert_eq!(status.phase, SyncPhase::Ready);
    assert!(observations.0.load(Ordering::SeqCst) > 0);
    let line = block_on(indexer.checkpoint(&own_network)).expect("checkpoint");
    assert_eq!(line, Some(block(BlockHeight(2))));
    assert!(
        held(&*pending, line.as_ref()).is_empty(),
        "instant finality, so nothing is held above the line"
    );
}

/// The pending facts an index exposes are the very blocks its next commit
/// writes: the same values, in the same order, and nothing else.
///
/// That is the whole of what is exposed, and it is why a consumer can compose a
/// pending-inclusive answer without the index answering any query. A sample
/// that was a projection of the held blocks rather than the blocks themselves
/// could disagree with what the store is about to hold, and a read composed
/// from it would show one thing above the line and another below it.
#[test]
fn the_pending_sample_is_the_blocks_the_next_commit_writes() {
    let own_network = network("sampled-then-committed");
    let owner = address(&own_network, "owner");
    let interpreter = Interpreter::of(&own_network);
    interpreter.credits(&block(BlockHeight(3)), &owner, 5);
    interpreter.credits(&block(BlockHeight(4)), &owner, 7);
    let source = Source::new(4).confirming(2);
    let repository = Repository::default();
    let service = Indexer::new(
        source.clone(),
        interpreter,
        repository.clone(),
        config(own_network.clone()),
    );
    let filters = vec![ForwardRange {
        address: owner.clone(),
        start_position: BlockPosition(0),
    }];

    let status = pass(&service, &filters).expect("the pass that holds two blocks");
    assert_eq!(status.checkpoint, Some(block(BlockHeight(2))));
    let before = repository.committed().len();
    let sampled = held(&service, status.checkpoint.as_ref());
    assert_eq!(
        sampled
            .iter()
            .map(|held| held.block().clone())
            .collect::<Vec<_>>(),
        [block(BlockHeight(3)), block(BlockHeight(4))],
        "everything above the line, oldest first"
    );
    assert_eq!(
        credited(&sampled, &owner),
        [base::Decimal::from(5_u64), base::Decimal::from(7_u64)],
        "with the facts a consumer folds"
    );

    // The chain advances by two, so both held blocks fall to the line.
    source.replace((0..=6).map(|height| block(BlockHeight(height))));
    let status = pass(&service, &filters).expect("the pass that commits them");
    assert_eq!(status.checkpoint, Some(block(BlockHeight(4))));

    assert_eq!(
        repository.committed()[before..],
        sampled[..],
        "the store was handed the blocks the sample held, unchanged"
    );
    assert!(
        held(&service, status.checkpoint.as_ref())
            .iter()
            .all(|held| held.block().position > BlockPosition(4)),
        "and what is held now is above the new line"
    );
}

/// A watch whose earlier outputs were not indexed must not lose later spends.
#[test]
fn a_spend_of_a_pre_birthday_output_is_reported_and_the_pass_completes() {
    let own_network = network("funded-chain");
    let owner = address(&own_network, "owner");
    let ghost = OutputKey {
        address: owner.clone(),
        output: OutputId {
            transaction: TransactionRef::new(own_network.clone(), "pre-birthday-funding"),
            index: 0,
        },
    };
    let interpreter = Interpreter::of(&own_network);
    interpreter.reports(
        BlockHeight(2),
        OutputChanges {
            spent: vec![ghost.clone()],
            ..OutputChanges::default()
        },
    );
    let service = Indexer::new(
        Source::new(2),
        interpreter,
        Repository::default(),
        config(own_network),
    );
    let filters = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(1),
    }];

    let statuses = pass(&service, &filters).expect("the pass must not fail");
    assert_eq!(statuses.phase, SyncPhase::Ready);
    assert_eq!(statuses.checkpoint, Some(block(BlockHeight(2))));
    assert_eq!(statuses.unmatched_spends, [ghost]);
}

/// A pass that accounts for every spend reports nothing, so a caller can read
/// an empty list as "this pass proved nothing incomplete".
#[test]
fn a_pass_that_matches_every_spend_reports_no_unknown_spend() {
    let own_network = network("matched-chain");
    let owner = address(&own_network, "owner");
    let created = IndexedOutput {
        id: OutputId {
            transaction: TransactionRef::new(own_network.clone(), "funding"),
            index: 0,
        },
        address: owner.clone(),
        asset: indexing::AssetId::native(own_network.chain.clone()),
        amount: base::Decimal::from(4_u64),
        evidence: vec![7],
        created_at: BlockHeight(1),
        coinbase: false,
    };
    let interpreter = Interpreter::of(&own_network);
    interpreter.reports(
        BlockHeight(1),
        OutputChanges {
            created: vec![created.clone()],
            ..OutputChanges::default()
        },
    );
    interpreter.reports(
        BlockHeight(2),
        OutputChanges {
            spent: vec![created.key()],
            ..OutputChanges::default()
        },
    );
    let service = Indexer::new(
        Source::new(2),
        interpreter,
        Repository::default(),
        config(own_network),
    );
    let filters = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(0),
    }];

    let statuses = pass(&service, &filters).expect("the pass must not fail");
    assert_eq!(statuses.phase, SyncPhase::Ready);
    assert!(statuses.unmatched_spends.is_empty());
}

/// An interpreter that answers about the block it was handed under a
/// reference that moved: the same block, reported one second later.
struct Relabelling(Interpreter);

impl BlockInterpreter for Relabelling {
    type Block = TestBlock;

    fn settlement(&self) -> indexing::FeeSettlement {
        self.0.settlement()
    }

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        let mut moved = block.clone();
        moved.0.timestamp = Some(1);
        self.0.inspect(&moved, addresses)
    }
}

/// The reference is compared after interpretation and before validation, so
/// facts about any block but the one that was fetched stop the pass and leave
/// nothing behind. The same index over the interpreter it wraps commits, which
/// is what says the refusal is about the reference and nothing else.
#[test]
fn facts_reported_under_a_moved_reference_are_refused_and_nothing_is_kept() {
    let own_network = network("relabelled");
    let repository = Repository::default();
    let service = Indexer::new(
        Source::new(4),
        Relabelling(Interpreter::of(&own_network)),
        repository.clone(),
        config(own_network.clone()),
    );

    let error = pass(&service, &[]).expect_err("the interpreter answered about another block");

    assert_eq!(
        error,
        IndexError::new(
            IndexErrorKind::InvalidBlock,
            "interpreter changed the block reference",
            false,
        )
    );
    assert_eq!(repository.stored(), (BTreeMap::new(), None));

    let faithful = Indexer::new(
        Source::new(4),
        Interpreter::of(&own_network),
        repository.clone(),
        config(own_network.clone()),
    );
    pass(&faithful, &[]).expect("the same chain, reported as it was fetched");
    assert_eq!(
        block_on(repository.checkpoint(&own_network)).expect("checkpoint lookup"),
        Some(block(BlockHeight(4)))
    );
}

/// Two transactions under one identifier never become a block's facts: the
/// interpreter's own output cannot be assembled, so the pass stops inside
/// `inspect` and the block that repeated it is not committed.
///
/// Block 3 carries one transaction and block 4 three, two of them under one
/// identifier with a third between them, so neither "one transaction per
/// block" nor "the repeat is adjacent" is what the refusal depends on.
#[test]
fn a_block_repeating_a_transaction_id_stops_the_pass_and_is_not_committed() {
    let own_network = network("repeated-id");
    let owner = address(&own_network, "owner");
    let interpreter = Interpreter::of(&own_network);
    interpreter.credits(&block(BlockHeight(3)), &owner, 2);
    interpreter.credits(&block(BlockHeight(4)), &owner, 5);
    interpreter.reports_draft(
        &block(BlockHeight(4)),
        indexing::ObservationDraft {
            transaction: indexing::Transaction {
                transaction_id: TransactionRef::new(own_network.clone(), "between"),
                movements: Vec::new(),
                fee: None,
            },
            transaction_order: 1,
            status: indexing::ObservationDraftStatus::Included,
        },
    );
    // `credits` names a transaction after its block, so this is the repeat.
    interpreter.credits(&block(BlockHeight(4)), &owner, 7);
    let repository = Repository::default();
    let service = Indexer::new(
        Source::new(4),
        interpreter,
        repository.clone(),
        config(own_network.clone()),
    );
    let watched = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(3),
    }];

    let error = pass(&service, &watched).expect_err("one identifier, two transactions");

    assert_eq!(
        error,
        IndexError::new(
            IndexErrorKind::InvalidBlock,
            "block contains a duplicate transaction",
            false,
        )
    );
    let (stored, _) = repository.stored();
    assert!(!stored.contains_key(&BlockHeight(4)));
}

#[test]
fn offline_reset_reloads_birthdays_and_discards_pending_and_native_cache() {
    let network = network("reset");
    let owner = address(&network, "owner");
    let source = Source::new(5).confirming(1);
    let repository = memory::Repository::new();
    let index = Indexer::new(
        source.clone(),
        Interpreter::of(&network),
        repository.clone(),
        config(network.clone()),
    );
    let selection = vec![ForwardRange {
        address: owner,
        start_position: BlockPosition(1),
    }];
    let before = pass(&index, &selection).unwrap();
    assert_eq!(before.checkpoint, Some(block(BlockHeight(4))));
    assert_eq!(before.pending_tip, Some(block(BlockHeight(5))));
    assert!(
        block_on(index.reset()).is_err(),
        "a live repository cannot be reset"
    );
    let watches = block_on(index.watched(&network)).unwrap();
    block_on(repository.purge_network(&network)).unwrap();
    block_on(index.reset()).unwrap();
    assert!(held(&index, None).is_empty());
    assert_eq!(block_on(index.watched(&network)).unwrap(), watches);
    source.replace((0..=3).map(|height| block(BlockHeight(height))));
    let after = block_on(index.sync()).unwrap();
    assert_eq!(after.checkpoint, Some(block(BlockHeight(2))));
    assert_eq!(after.pending_tip, Some(block(BlockHeight(3))));
    assert!(
        source
            .requests()
            .iter()
            .filter(|h| **h == BlockHeight(2))
            .count()
            >= 2
    );
}
