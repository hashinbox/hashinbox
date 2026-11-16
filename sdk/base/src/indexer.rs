//! [`Indexer`]: one network's chain into its [`BlockStore`], a step at a time.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::ensure;

use crate::{
    Addressed, Asset, Balances, BlockHeader, BlockStore, Chained, Network, Networked, NotServed,
    Tips, WalletIndex, WalletIndexState, WalletIndexStore,
};

/// Blocks fetched per step unless [`Indexer::with_batch`] says otherwise.
pub const DEFAULT_BATCH: u64 = 100;

/// Indexes settled per step at most.
const SETTLES: usize = 8;

/// What one [`Indexer::next`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Kept blocks through this one.
    Indexed(BlockHeader),
    /// Reorged kept blocks from this position: the chain took them back.
    Reorged(u64),
    /// Nothing to fetch.
    Idle,
}

/// Where a new watch starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Birthday {
    /// At the chain's produced tip: no history.
    Fresh,
    /// At this position: history from there on.
    At(u64),
}

/// What an [`Indexer`] did since it was made: how healthy it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Blocks kept.
    pub blocks: u64,
    /// The last block kept.
    pub last: Option<BlockHeader>,
    /// The chain's produced tip at the last step; with `last`, the lag.
    pub tip: Option<u64>,
    /// The chain's confirmed (final) tip at the last step.
    pub confirmed: Option<u64>,
    /// Kept blocks reorged because the chain took them back.
    pub reorgs: u64,
    /// Addresses watched.
    pub watched: u64,
    /// Failed steps, and failed settles (balance reads or their writes).
    pub failures: u64,
    /// The last failure, cleared by the next kept block.
    pub error: Option<String>,
}

impl Stats {
    /// Counts `error` in `failures`; it is the last `error`.
    fn fail(&mut self, error: &anyhow::Error) {
        self.failures += 1;
        self.error = Some(format!("{error:#}"));
    }
}

/// [`Stats`] as they change, behind their own lock.
#[derive(Default)]
struct StatsWithMutex(Mutex<Stats>);

impl StatsWithMutex {
    fn get(&self) -> std::sync::MutexGuard<'_, Stats> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Indexes one network: fetches blocks up to the produced tip and keeps them,
/// marking kept blocks the chain took back reorged. The caller steps it
/// ([`Indexer::next`]), about every [`Indexer::interval`].
pub struct Indexer {
    chain: Arc<dyn Chained>,
    blocks: Arc<dyn BlockStore>,
    indexes: Arc<dyn WalletIndexStore>,
    interval: Duration,
    batch: u64,
    balances: Option<Arc<dyn Balances>>,
    max_depth: Option<u64>,
    stats: StatsWithMutex,
}

impl Indexer {
    /// Indexes `chain` into `blocks`, for the addresses of `indexes`, stepped
    /// about every `interval` once at the tip.
    #[must_use]
    pub fn new(
        chain: Arc<dyn Chained>,
        blocks: Arc<dyn BlockStore>,
        indexes: Arc<dyn WalletIndexStore>,
        interval: Duration,
    ) -> Self {
        Self {
            chain,
            blocks,
            indexes,
            interval,
            batch: DEFAULT_BATCH,
            balances: None,
            max_depth: None,
            stats: StatsWithMutex::default(),
        }
    }

    /// How long to wait between steps once at the tip.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Fetches at most `batch` positions per step (at least one).
    #[must_use]
    pub fn with_batch(mut self, batch: u64) -> Self {
        self.batch = batch.max(1);
        self
    }

    /// Reads balances with `balances`: nothing at or below the chain's final
    /// block is read. A new watch starts there, and each step first settles
    /// ([`Self::settle`]) every index whose next block is at or below it,
    /// then reads on above it. It replaces `max_depth`
    /// ([`Self::with_max_depth`]).
    #[must_use]
    pub fn with_balances(mut self, balances: Arc<dyn Balances>) -> Self {
        self.balances = Some(balances);
        self
    }

    /// Never reads more than `max_depth` positions below the tip: an index
    /// further behind jumps ([`Self::jump`]). `None`: no limit. Without
    /// balances only ([`Self::with_balances`]).
    #[must_use]
    pub const fn with_max_depth(mut self, max_depth: Option<u64>) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Indexes `asset` of `address` from `birthday`, its `requested_from`;
    /// [`Indexer::next`] backfills it. With balances
    /// ([`Indexer::with_balances`]) it starts at the chain's final block
    /// instead, and the next step settles it.
    ///
    /// # Errors
    ///
    /// When `address` or `asset` is of another network, or the chain or
    /// store fails.
    pub async fn watch(
        &self,
        address: &dyn Addressed,
        asset: &Asset,
        birthday: Birthday,
    ) -> anyhow::Result<()> {
        let address = address.address();
        ensure!(
            address.network == *self.network() && asset.network() == self.network(),
            "{:?} or {asset:?} is not of {:?}",
            address.network,
            self.network()
        );
        let requested_from = match birthday {
            Birthday::Fresh => self.chain.tips().await?.produced,
            Birthday::At(position) => position,
        };
        let birthday = match self.balances {
            Some(_) => self.chain.tips().await?.confirmed,
            None => requested_from,
        };
        let index = WalletIndex {
            address: address.clone(),
            asset: asset.clone(),
            requested_from,
            birthday,
            indexed_at: None,
            state: WalletIndexState::Indexing,
            reason: None,
        };
        self.indexes.insert(&index).await?;
        self.stats.get().watched += 1;
        Ok(())
    }

    /// What it did so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats.get().clone()
    }

    /// One step of at most a batch: from the furthest-behind
    /// [`WalletIndexState::Indexing`] index, or else from the last kept
    /// block. When the chain no longer has the last kept block, marks it
    /// reorged; the next step checks the one below. Never below the line
    /// ([`Self::line`]): with balances, it first settles a few indexes not
    /// past the chain's final block ([`Self::settle`]); without, an index
    /// behind the tip less `max_depth` jumps there ([`Self::jump`]). When
    /// the node no longer serves a block (pruned history), an index that has
    /// not started starts at the first block it serves; one that has started
    /// becomes [`WalletIndexState::NoData`] and the step fails.
    ///
    /// # Errors
    ///
    /// When the chain or a store fails, the chain changes mid-fetch
    /// (retry), or the node no longer serves a block a started index or the
    /// network needs ([`NotServed`]); counted in [`Stats::failures`].
    pub async fn next(&self) -> anyhow::Result<Next> {
        let next = self.step().await;
        if let Err(error) = &next {
            self.stats.get().fail(error);
        }
        next
    }

    /// Settles at most [`SETTLES`] indexing indexes whose next block is at
    /// or below `confirmed`, the chain's final block: each reads its balance
    /// at a final block `S` at or above it and, once the chain answers and
    /// serves `S`, writes it ([`WalletIndexStore::settle`]); it reads on from
    /// `S + 1`. One not answerable yet stays as it is for the next step; a
    /// failure is counted in [`Stats::failures`], never failing the step.
    async fn settle(&self, balances: &dyn Balances, confirmed: u64) {
        let below = confirmed.saturating_add(1);
        let behind = self.indexes.find_behind(self.network(), below, SETTLES);
        let behind = match behind.await {
            Ok(behind) => behind,
            Err(error) => return self.stats.get().fail(&error),
        };
        for index in &behind {
            if let Err(error) = self.settle_one(balances, index, confirmed).await {
                self.stats.get().fail(&error);
            }
        }
    }

    /// [`Self::settle`] of one `index`, at a final block at or above `at`.
    async fn settle_one(
        &self,
        balances: &dyn Balances,
        index: &WalletIndex,
        at: u64,
    ) -> anyhow::Result<()> {
        let read = balances.balance(&index.address, &index.asset, at);
        let Some(holding) = read.await? else {
            return Ok(());
        };
        let Some(block) = self.chain.block(holding.position).await? else {
            return Ok(());
        };
        self.indexes.settle(index, &holding, &block.header).await?;
        Ok(())
    }

    /// [`Self::fetch`]; when the node no longer serves a block an index
    /// needs, [`Self::skip`] and, when every such index moved, fetch again.
    async fn step(&self) -> anyhow::Result<Next> {
        match self.fetch().await {
            Err(error) => match NotServed::find(&error) {
                Some(gap) if self.skip(gap).await? => self.fetch().await,
                _ => Err(error),
            },
            next => next,
        }
    }

    /// Moves the indexes stuck behind `gap`, a block the node no longer
    /// serves, out of the way. When one needs it (or the block after it),
    /// finds the first block the node serves; below it nothing is served.
    /// Every index whose next block is below that and has not started
    /// starts there; every one that has started becomes
    /// [`WalletIndexState::NoData`]. Whether the step can go on: one moved,
    /// and none had started.
    ///
    /// # Errors
    ///
    /// When the chain or the store fails.
    async fn skip(&self, gap: NotServed) -> anyhow::Result<bool> {
        let network = self.network();
        let needs = self
            .indexes
            .find_behind(network, gap.position.saturating_add(2), 1);
        if needs.await?.is_empty() {
            return Ok(false);
        }
        let tip = self.chain.tips().await?.produced;
        let start = first_served(&*self.chain, gap.position, tip).await?;
        let (mut moved, mut stopped) = (false, false);
        for index in &self.indexes.find_behind(network, start, usize::MAX).await? {
            match index.next() {
                Some(_) if index.indexed_at.is_none() => {
                    self.indexes.move_birthday(index, start).await?;
                    moved = true;
                }
                Some(position) => {
                    let reason = NotServed { position }.to_string();
                    self.indexes
                        .set_state(index, WalletIndexState::NoData, Some(&reason))
                        .await?;
                    stopped = true;
                }
                None => {}
            }
        }
        Ok(moved && !stopped)
    }

    async fn fetch(&self) -> anyhow::Result<Next> {
        let network = self.network();
        let last = self.blocks.find_last(network).await?;
        // The tip first: a network with nothing to index still shows it.
        let tips = self.chain.tips().await?;
        let tip = tips.produced;
        {
            let mut stats = self.stats.get();
            stats.tip = Some(tip);
            stats.confirmed = Some(tips.confirmed);
        }
        let line = self.line(tips);
        if let Some(line) = line
            && let Some(reorged) = self.check(line, last.as_ref()).await?
        {
            return Ok(reorged);
        }
        if let Some(balances) = &self.balances {
            self.settle(&**balances, tips.confirmed).await;
        } else if let Some(line) = line {
            self.jump(line).await?;
        }
        // After the settles and jumps: no index left to read is below the line.
        let floor = line.unwrap_or(0);
        let through = last.as_ref().map(|last| last.position);
        let lagging = self.indexes.find_next(network, floor, through).await?;
        // Forward starts at the last kept block, to see it is still on the chain.
        let Some(start) = lagging.or(through) else {
            return Ok(Next::Idle);
        };
        let start = start.max(floor);
        let end = tip.min(start.saturating_add(self.batch - 1));
        let run = self.chain.blocks(start, end).await?;
        // The run may end early (the node has no more yet): only the
        // positions it reached can tell whether the last kept block is gone.
        let Some(reached) = run.last().map(|header| header.position) else {
            return Ok(Next::Idle);
        };

        // One below the start is the line's to check (`Self::check`).
        if let Some(last) = &last
            && (start..=reached).contains(&last.position)
            && !run.contains(last)
        {
            return self.reorg(last).await;
        }
        let Some(header) = run.last().cloned() else {
            return Ok(Next::Idle);
        };
        if lagging.is_none() && last.as_ref() == Some(&header) {
            return Ok(Next::Idle);
        }
        self.blocks.insert(&run).await?;
        let mut stats = self.stats.get();
        stats.blocks += run.len() as u64;
        stats.last = Some(header.clone());
        stats.error = None;
        drop(stats);
        Ok(Next::Indexed(header))
    }

    /// The line nothing below is read: with balances, the one above the
    /// chain's final block (an index at or below it settles instead);
    /// without, the tip less `max_depth` (an index below it jumps); `None`:
    /// no line.
    fn line(&self, tips: Tips) -> Option<u64> {
        if self.balances.is_some() {
            return Some(tips.confirmed.saturating_add(1));
        }
        self.max_depth
            .map(|depth| tips.produced.saturating_sub(depth))
    }

    /// When `last`, the last kept block, is below `line`, checks the chain
    /// still has it: when not, marks it reorged instead (the next step
    /// checks the one below). With it, every leg kept below is final
    /// history. A node that no longer serves it ([`NotServed`], pruned
    /// history) counts as having it: it is far below the final tip.
    async fn check(&self, line: u64, last: Option<&BlockHeader>) -> anyhow::Result<Option<Next>> {
        let Some(last) = last.filter(|last| last.position < line) else {
            return Ok(None);
        };
        let gone = match self.chain.block(last.position).await {
            Ok(block) => block.is_none_or(|block| block.header != *last),
            Err(error) if NotServed::find(&error).is_some() => false,
            Err(error) => return Err(error),
        };
        if !gone {
            return Ok(None);
        }
        self.reorg(last).await.map(Some)
    }

    /// Moves every indexing index whose next is below `floor`, the jump line
    /// (the tip less `max_depth`), up to it ([`WalletIndexStore::restart`]):
    /// the gap below is never read.
    async fn jump(&self, floor: u64) -> anyhow::Result<()> {
        let behind = self.indexes.find_behind(self.network(), floor, usize::MAX);
        for index in &behind.await? {
            self.indexes.restart(index, floor).await?;
        }
        Ok(())
    }

    /// Marks `last`, the last kept block, reorged: the chain took it back.
    async fn reorg(&self, last: &BlockHeader) -> anyhow::Result<Next> {
        self.blocks.delete(self.network(), last.position).await?;
        self.stats.get().reorgs += 1;
        Ok(Next::Reorged(last.position))
    }
}

/// The first position in `from..=tip` whose block the node serves, the
/// block at `from` not served. Pruning removes a prefix of history, so
/// "served" flips once: a binary search of about `log2(tip - from)` full
/// block reads.
///
/// # Errors
///
/// [`NotServed`] when not even the tip is served; a failed read.
async fn first_served(chain: &dyn Chained, from: u64, tip: u64) -> anyhow::Result<u64> {
    if tip <= from || !served(chain, tip).await? {
        return Err(NotServed { position: tip }.into());
    }
    // The block at `low` is not served; the one at `high` is.
    let (mut low, mut high) = (from, tip);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if served(chain, middle).await? {
            high = middle;
        } else {
            low = middle;
        }
    }
    Ok(high)
}

/// Whether the node serves the block at `position` (a skipped one counts).
async fn served(chain: &dyn Chained, position: u64) -> anyhow::Result<bool> {
    match chain.block(position).await {
        Ok(_) => Ok(true),
        Err(error) if NotServed::find(&error).is_some() => Ok(false),
        Err(error) => Err(error),
    }
}

impl Networked for Indexer {
    fn network(&self) -> &Network {
        self.chain.network()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::{
        Address, Block, BlockHash, BlockParent, Blocks, Chain, Future, Tips,
        transaction::Transactions,
    };

    /// A chain with a block at every position `0..=TIP`, serving those from
    /// `first` on; and the stores, in memory.
    struct Fake {
        network: Network,
        first: AtomicU64,
        blocks: Mutex<Vec<BlockHeader>>,
        indexes: Mutex<Vec<WalletIndex>>,
    }

    const TIP: u64 = 50;

    fn header(position: u64) -> BlockHeader {
        BlockHeader {
            position,
            height: position,
            hash: BlockHash(position.to_be_bytes().to_vec()),
            parent: position.checked_sub(1).map(|parent| BlockParent {
                position: parent,
                hash: BlockHash(parent.to_be_bytes().to_vec()),
            }),
            timestamp: None,
        }
    }

    impl Networked for Fake {
        fn network(&self) -> &Network {
            &self.network
        }
    }

    impl Chained for Fake {
        fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
            Box::pin(async {
                Ok(Tips {
                    produced: TIP,
                    confirmed: TIP,
                })
            })
        }

        fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
            let served = position >= self.first.load(Ordering::SeqCst);
            let block = Block {
                network: self.network.clone(),
                header: header(position),
                transactions: Transactions::default(),
                rewards: Vec::new(),
            };
            Box::pin(async move {
                anyhow::ensure!(served, NotServed { position });
                Ok((position <= TIP).then_some(block))
            })
        }
    }

    impl BlockStore for Fake {
        /// Keeps new blocks; moves the indexes each applies to, as the
        /// server's store does.
        fn insert<'a>(&'a self, run: &'a Blocks) -> Future<'a, anyhow::Result<()>> {
            let mut blocks = self.blocks.lock().unwrap();
            let mut indexes = self.indexes.lock().unwrap();
            for block in run.iter() {
                let at = &block.header;
                for index in indexes.iter_mut() {
                    let applies = index.state == WalletIndexState::Indexing
                        && index.birthday <= at.position
                        && match &index.indexed_at {
                            Some(through) => Some(through.position) == at.position.checked_sub(1),
                            None => at
                                .parent
                                .as_ref()
                                .is_none_or(|parent| parent.position < index.birthday),
                        };
                    if applies {
                        index.indexed_at = Some(at.clone());
                    }
                }
                if !blocks.contains(at) {
                    blocks.push(at.clone());
                }
            }
            Box::pin(async { Ok(()) })
        }

        fn find_last<'a>(
            &'a self,
            _network: &'a Network,
        ) -> Future<'a, anyhow::Result<Option<BlockHeader>>> {
            let last = self
                .blocks
                .lock()
                .unwrap()
                .iter()
                .max_by_key(|h| h.position)
                .cloned();
            Box::pin(async { Ok(last) })
        }

        fn delete<'a>(
            &'a self,
            _network: &'a Network,
            _from: u64,
        ) -> Future<'a, anyhow::Result<()>> {
            unimplemented!("no reorg here")
        }
    }

    impl WalletIndexStore for Fake {
        fn insert<'a>(&'a self, index: &'a WalletIndex) -> Future<'a, anyhow::Result<()>> {
            self.indexes.lock().unwrap().push(index.clone());
            Box::pin(async { Ok(()) })
        }

        fn find_all<'a>(
            &'a self,
            _network: &'a Network,
        ) -> Future<'a, anyhow::Result<Vec<WalletIndex>>> {
            let all = self.indexes.lock().unwrap().clone();
            Box::pin(async { Ok(all) })
        }

        fn move_birthday<'a>(
            &'a self,
            index: &'a WalletIndex,
            birthday: u64,
        ) -> Future<'a, anyhow::Result<()>> {
            self.change(index, |kept| {
                if kept.indexed_at.is_none() {
                    kept.birthday = birthday;
                }
            });
            Box::pin(async { Ok(()) })
        }

        fn set_state<'a>(
            &'a self,
            index: &'a WalletIndex,
            state: WalletIndexState,
            reason: Option<&'a str>,
        ) -> Future<'a, anyhow::Result<()>> {
            self.change(index, |kept| {
                kept.state = state;
                kept.reason = reason.map(str::to_owned);
            });
            Box::pin(async { Ok(()) })
        }

        fn settle<'a>(
            &'a self,
            _index: &'a WalletIndex,
            _holding: &'a crate::Holding,
            _header: &'a BlockHeader,
        ) -> Future<'a, anyhow::Result<bool>> {
            unimplemented!("no balances here")
        }

        fn restart<'a>(
            &'a self,
            _index: &'a WalletIndex,
            _birthday: u64,
        ) -> Future<'a, anyhow::Result<()>> {
            unimplemented!("no jump here")
        }
    }

    impl Fake {
        fn change(&self, index: &WalletIndex, change: impl Fn(&mut WalletIndex)) {
            let mut indexes = self.indexes.lock().unwrap();
            indexes
                .iter_mut()
                .filter(|kept| kept.address == index.address && kept.asset == index.asset)
                .for_each(change);
        }

        fn index(&self, address: &str) -> WalletIndex {
            self.indexes
                .lock()
                .unwrap()
                .iter()
                .find(|index| index.address.value == address)
                .cloned()
                .unwrap()
        }
    }

    /// An indexer over a fake serving from `first`, fetching `batch` a step.
    fn indexer(first: u64, batch: u64) -> (Arc<Fake>, Indexer) {
        let fake = Arc::new(Fake {
            network: Network::new(Chain::from_static("test"), "fake"),
            first: AtomicU64::new(first),
            blocks: Mutex::default(),
            indexes: Mutex::default(),
        });
        let indexer = Indexer::new(fake.clone(), fake.clone(), fake.clone(), Duration::ZERO)
            .with_batch(batch);
        (fake, indexer)
    }

    async fn watch(indexer: &Indexer, address: &str, from: u64) {
        let network = indexer.network().clone();
        let asset = Asset::native(network.clone());
        let address = Address::new(network, address);
        indexer
            .watch(&address, &asset, Birthday::At(from))
            .await
            .unwrap();
    }

    fn at(index: &WalletIndex) -> Option<u64> {
        index.indexed_at.as_ref().map(|header| header.position)
    }

    #[tokio::test]
    async fn a_network_with_nothing_to_index_still_reads_its_tip() {
        let (_fake, indexer) = indexer(0, 100);
        assert_eq!(indexer.next().await.unwrap(), Next::Idle);
        let stats = indexer.stats();
        assert_eq!((stats.tip, stats.confirmed), (Some(TIP), Some(TIP)));
    }

    #[tokio::test]
    async fn an_unstarted_index_starts_at_the_first_served_block() {
        let (fake, indexer) = indexer(20, 100);
        watch(&indexer, "a", 5).await;
        assert_eq!(indexer.next().await.unwrap(), Next::Indexed(header(TIP)));
        let index = fake.index("a");
        assert_eq!((index.requested_from, index.birthday), (5, 20));
        assert_eq!(
            (index.state, at(&index)),
            (WalletIndexState::Indexing, Some(TIP))
        );
    }

    #[tokio::test]
    async fn a_started_index_with_a_gap_has_no_data_and_fails_the_step() {
        let (fake, indexer) = indexer(0, 10);
        watch(&indexer, "a", 0).await;
        indexer.next().await.unwrap();
        assert_eq!(at(&fake.index("a")), Some(9));
        // A fresh index behind it, and the node prunes past both.
        watch(&indexer, "b", 3).await;
        fake.first.store(30, Ordering::SeqCst);
        let error = indexer.next().await.unwrap_err();
        assert_eq!(NotServed::find(&error), Some(NotServed { position: 3 }));
        let (a, b) = (fake.index("a"), fake.index("b"));
        assert_eq!(a.state, WalletIndexState::NoData);
        assert_eq!(a.reason.as_deref(), Some("node no longer serves block 10"));
        assert_eq!(at(&a), Some(9));
        assert_eq!(
            (b.state, b.birthday, b.requested_from),
            (WalletIndexState::Indexing, 30, 3)
        );
        assert!(indexer.stats().error.is_some());
    }

    #[tokio::test]
    async fn a_paused_index_is_skipped_and_resumes_without_a_gap() {
        let (fake, indexer) = indexer(0, 10);
        watch(&indexer, "a", 0).await;
        watch(&indexer, "b", 0).await;
        indexer.next().await.unwrap();
        let b = fake.index("b");
        assert_eq!(at(&b), Some(9));
        fake.set_state(&b, WalletIndexState::Paused, None)
            .await
            .unwrap();
        while indexer.next().await.unwrap() != Next::Idle {}
        assert_eq!(at(&fake.index("a")), Some(TIP));
        assert_eq!(at(&fake.index("b")), Some(9));
        fake.set_state(&b, WalletIndexState::Indexing, None)
            .await
            .unwrap();
        while indexer.next().await.unwrap() != Next::Idle {}
        // The store moves an index only onto the child of its block.
        assert_eq!(at(&fake.index("b")), Some(TIP));
    }
}
