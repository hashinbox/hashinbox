//! [`Indexer`]: one network's chain into its [`BlockStore`], a step at a
//! time, moving every kind of index ([`Indexes`]) over it.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use crate::{BlockHeader, BlockStore, Chained, Future, Network, Networked, NotServed, Skipped};

/// Blocks fetched per step unless [`Indexer::with_batch`] says otherwise.
pub const DEFAULT_BATCH: u64 = 100;

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

/// What an [`Indexer`] did since it was made: how healthy it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Blocks kept.
    pub blocks: u64,
    /// The last block kept.
    pub last: Option<BlockHeader>,
    /// The chain's newest block at the last step; with `last`, the lag.
    pub tip: Option<u64>,
    /// The confirmed position at the last step: `confirmations` below
    /// `tip` ([`crate::Tips::confirmed`]).
    pub confirmed: Option<u64>,
    /// The chain's line ([`Chained::line`]) at the last step: nothing below
    /// it is read.
    pub line: Option<u64>,
    /// Kept blocks reorged because the chain took them back.
    pub reorgs: u64,
    /// Failed steps, and failed closes ([`Indexes::close`]).
    pub failures: u64,
    /// The last failure, cleared by the next kept block of a step whose
    /// closes all succeeded.
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

/// One kind of index over a network's blocks: a part of its [`Indexer`].
/// Keeping blocks moves its items ([`BlockStore::insert`]); the part says
/// where they need reading and closes their gaps.
pub trait Indexes: Send + Sync {
    /// The lowest next block of its indexing items, at or above `from` and
    /// at or below `through` (`None`: no bound): where the next run may
    /// start.
    ///
    /// # Errors
    ///
    /// When its store cannot read.
    fn next<'a>(
        &'a self,
        from: u64,
        through: Option<u64>,
    ) -> Future<'a, anyhow::Result<Option<u64>>>;

    /// Closes the gaps of its items whose next block is below `line`, the
    /// chain's, `confirmed` its confirmed position ([`crate::Tips::confirmed`]).
    ///
    /// # Errors
    ///
    /// When the chain or its store fails; the step goes on.
    fn close<'a>(&'a self, line: u64, confirmed: u64) -> Future<'a, anyhow::Result<()>>;

    /// Moves its items stuck below `below`, behind a block the node no
    /// longer serves: an unstarted one starts at `start`, a started one is
    /// [`crate::IndexState::NoData`].
    ///
    /// # Errors
    ///
    /// When its store fails.
    fn skip<'a>(&'a self, below: u64, start: u64) -> Future<'a, anyhow::Result<Skipped>>;
}

/// Indexes one network: fetches blocks up to the produced tip and keeps them,
/// deleting kept blocks the chain took back, and has each of its
/// parts close the gaps of its items below the chain's line. The caller
/// steps it ([`Indexer::next`]), about every [`Indexer::interval`]. Each
/// step is logged as `index.step` (`start end blocks ms outcome`), each
/// reorg as `index.reorg` (`from`); the caller's span names the network.
pub struct Indexer {
    chain: Arc<dyn Chained>,
    /// Keeping blocks advances the items of every part, in the same write
    /// as the blocks; deleting them rewinds every part.
    blocks: Arc<dyn BlockStore>,
    parts: Vec<Arc<dyn Indexes>>,
    interval: Duration,
    batch: u64,
    stats: StatsWithMutex,
}

impl Indexer {
    /// Indexes `chain` into `blocks`, for the items of `parts`, stepped
    /// about every `interval` once at the tip.
    #[must_use]
    pub fn new(
        chain: Arc<dyn Chained>,
        blocks: Arc<dyn BlockStore>,
        parts: Vec<Arc<dyn Indexes>>,
        interval: Duration,
    ) -> Self {
        Self {
            chain,
            blocks,
            parts,
            interval,
            batch: DEFAULT_BATCH,
            stats: StatsWithMutex::default(),
        }
    }

    /// How long to wait between steps once at the tip.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// The chain it reads: what a target becomes an index with.
    #[must_use]
    pub fn chain(&self) -> Arc<dyn Chained> {
        self.chain.clone()
    }

    /// Fetches at most `batch` positions per step (at least one).
    #[must_use]
    pub fn with_batch(mut self, batch: u64) -> Self {
        self.batch = batch.max(1);
        self
    }

    /// What it did so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats.get().clone()
    }

    /// One step of at most a batch: from the furthest-behind indexing item
    /// of any part, or else from the last kept block. When the chain no
    /// longer has the last kept block, deletes it (a reorg); the next step
    /// checks the one below. Never below the chain's line
    /// ([`Chained::line`]): each part first closes the gaps of its items
    /// below it ([`Indexes::close`]; a failure is counted, never failing the
    /// step). When the node no longer serves a block (pruned history), an
    /// item that has not started starts at the first block it serves; one
    /// that has started becomes [`crate::IndexState::NoData`] and the step
    /// fails.
    ///
    /// # Errors
    ///
    /// When the chain or a store fails, the chain changes mid-fetch
    /// (retry), or the node no longer serves a block a started item or the
    /// network needs ([`NotServed`]); counted in [`Stats::failures`].
    pub async fn next(&self) -> anyhow::Result<Next> {
        let (started, kept) = (Instant::now(), self.stats.get().blocks);
        let mut range = None;
        let next = self.step(&mut range).await;
        if let Err(error) = &next {
            self.stats.get().fail(error);
        }
        let shown = |position: Option<u64>| position.map_or("-".to_owned(), |p| p.to_string());
        let (start, end) = (shown(range.map(|r| r.0)), shown(range.map(|r| r.1)));
        let blocks = self.stats.get().blocks - kept;
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let message = "Index step finished";
        match &next {
            Ok(Next::Indexed(_)) => {
                tracing::info!(target: "index.step", start, end, blocks, ms, outcome = "indexed", "{message}");
            }
            Ok(Next::Reorged(_)) => {
                tracing::info!(target: "index.step", start, end, blocks, ms, outcome = "reorged", "{message}");
            }
            Ok(Next::Idle) => {
                tracing::debug!(target: "index.step", start, end, blocks, ms, outcome = "idle", "{message}");
            }
            Err(_) => {
                tracing::warn!(target: "index.step", start, end, blocks, ms, outcome = "failed", "{message}");
            }
        }
        next
    }

    /// [`Self::fetch`]; when the node no longer serves a block an item
    /// needs, [`Self::skip`] and, when every such item moved, fetch again.
    /// `range`: the positions last asked for.
    async fn step(&self, range: &mut Option<(u64, u64)>) -> anyhow::Result<Next> {
        match self.fetch(range).await {
            Err(error) => match NotServed::find(&error) {
                Some(gap) if self.skip(gap).await? => self.fetch(range).await,
                _ => Err(error),
            },
            next => next,
        }
    }

    /// Moves the items stuck behind `gap`, a block the node no longer
    /// serves, out of the way. When one needs it (or the block after it),
    /// finds the first block the node serves; below it nothing is served.
    /// Every item whose next block is below that and has not started starts
    /// there; every one that has started becomes
    /// [`crate::IndexState::NoData`] ([`Indexes::skip`]). Whether the step
    /// can go on: one moved, and none had started.
    ///
    /// # Errors
    ///
    /// When the chain or a store fails.
    async fn skip(&self, gap: NotServed) -> anyhow::Result<bool> {
        let mut needs = false;
        for part in &self.parts {
            needs |= part
                .next(0, Some(gap.position.saturating_add(1)))
                .await?
                .is_some();
        }
        if !needs {
            return Ok(false);
        }
        let tip = self.chain.tips().await?.position;
        let start = first_served(&*self.chain, gap.position, tip).await?;
        let mut skipped = Skipped::default();
        for part in &self.parts {
            skipped = skipped.and(part.skip(start, start).await?);
        }
        Ok(skipped.moved && !skipped.stopped)
    }

    async fn fetch(&self, range: &mut Option<(u64, u64)>) -> anyhow::Result<Next> {
        let network = self.network();
        let last = self.blocks.find_last(network).await?;
        // The tip first: a network with nothing to index still shows it.
        let tips = self.chain.tips().await?;
        let tip = tips.position;
        let confirmations = self.chain.confirmations();
        let confirmed = tips.confirmed(confirmations);
        let line = self.chain.line().at(tips, confirmations);
        {
            let mut stats = self.stats.get();
            stats.tip = Some(tip);
            stats.confirmed = Some(confirmed);
            stats.line = Some(line);
        }
        if let Some(reorged) = self.check(line, last.as_ref()).await? {
            return Ok(reorged);
        }
        let mut closed = true;
        for part in &self.parts {
            if let Err(error) = part.close(line, confirmed).await {
                self.stats.get().fail(&error);
                closed = false;
            }
        }
        // After the closes: no item left to read is below the line.
        let through = last.as_ref().map(|last| last.position);
        let mut lagging = None;
        for part in &self.parts {
            lagging = lagging
                .into_iter()
                .chain(part.next(line, through).await?)
                .min();
        }
        // Forward starts at the last kept block, to see it is still on the chain.
        let Some(start) = lagging.or(through) else {
            return Ok(Next::Idle);
        };
        let start = start.max(line);
        let end = tip.min(start.saturating_add(self.batch - 1));
        *range = Some((start, end));
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
        if closed {
            stats.error = None;
        }
        drop(stats);
        Ok(Next::Indexed(header))
    }

    /// When `last`, the last kept block, is below `line`, checks the chain
    /// still has it: when not, deletes it instead (the next step
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

    /// Deletes `last`, the last kept block, and above: the chain took it
    /// back.
    async fn reorg(&self, last: &BlockHeader) -> anyhow::Result<Next> {
        self.blocks.delete(self.network(), last.position).await?;
        self.stats.get().reorgs += 1;
        tracing::warn!(target: "index.reorg", from = last.position, "Kept blocks reorged");
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
    use super::*;
    use crate::{
        Address, Asset, Birthday, Chain, IndexState, WalletIndex, WalletIndexStore,
        WalletIndexTarget,
        testing::{
            Memory, Served,
            fixtures::{header, quiet},
            idle, indexer as real,
        },
    };

    const TIP: u64 = 50;

    /// A chain with a block at every position `0..=TIP`, serving those from
    /// `first`; the stores in memory; an indexer over both fetching `batch`
    /// a step.
    fn indexer(first: u64, batch: u64) -> (Served, Arc<Memory>, Indexer) {
        let network = Network::new(Chain::from_static("test"), "fake");
        let chain = Served::new(quiet(&network, TIP), 0);
        chain.prune(first);
        let memory = Arc::new(Memory::new());
        let indexer = real(chain.clone(), memory.clone(), None, None).with_batch(batch);
        (chain, memory, indexer)
    }

    /// Watches the native asset of `address` from `from`.
    async fn watch(chain: &Served, memory: &Memory, address: &str, from: u64) {
        let network = chain.network().clone();
        let asset = Asset::native(network.clone());
        let address = Address::new(network, address);
        let target = WalletIndexTarget::new(Arc::new(chain.clone()), address, asset)
            .with_birthday(Birthday::At(from));
        WalletIndexStore::insert(memory, &target).await.unwrap();
    }

    const fn at(index: &WalletIndex) -> Option<u64> {
        index.progress.indexed_at
    }

    #[tokio::test]
    async fn a_network_with_nothing_to_index_still_reads_its_tip() {
        let (_chain, _memory, indexer) = indexer(0, 100);
        assert_eq!(indexer.next().await.unwrap(), Next::Idle);
        let stats = indexer.stats();
        assert_eq!((stats.tip, stats.confirmed), (Some(TIP), Some(TIP)));
    }

    #[tokio::test]
    async fn an_unstarted_index_starts_at_the_first_served_block() {
        let (chain, memory, indexer) = indexer(20, 100);
        watch(&chain, &memory, "a", 5).await;
        assert_eq!(indexer.next().await.unwrap(), Next::Indexed(header(TIP, 0)));
        let index = memory.wallet_index("a");
        assert_eq!((index.requested_from, index.birthday), (5, 20));
        assert_eq!(
            (index.progress.state, at(&index)),
            (IndexState::Indexing, Some(TIP))
        );
    }

    #[tokio::test]
    async fn a_started_index_with_a_gap_has_no_data_and_fails_the_step() {
        let (chain, memory, indexer) = indexer(0, 10);
        watch(&chain, &memory, "a", 0).await;
        indexer.next().await.unwrap();
        assert_eq!(at(&memory.wallet_index("a")), Some(9));
        // A fresh index behind it, and the node prunes past both.
        watch(&chain, &memory, "b", 3).await;
        chain.prune(30);
        let error = indexer.next().await.unwrap_err();
        assert_eq!(NotServed::find(&error), Some(NotServed { position: 3 }));
        let (a, b) = (memory.wallet_index("a"), memory.wallet_index("b"));
        assert_eq!(a.progress.state, IndexState::NoData);
        assert_eq!(
            a.progress.reason.as_deref(),
            Some("node no longer serves block 10")
        );
        assert_eq!(at(&a), Some(9));
        assert_eq!(
            (b.progress.state, b.birthday, b.requested_from),
            (IndexState::Indexing, 30, 3)
        );
        assert!(indexer.stats().error.is_some());
    }

    #[tokio::test]
    async fn a_paused_index_is_skipped_and_resumes_without_a_gap() {
        let (chain, memory, indexer) = indexer(0, 10);
        watch(&chain, &memory, "a", 0).await;
        watch(&chain, &memory, "b", 0).await;
        indexer.next().await.unwrap();
        let b = memory.wallet_index("b");
        assert_eq!(at(&b), Some(9));
        let set = |state| WalletIndexStore::set_state(&*memory, &b, state, None);
        set(IndexState::Paused).await.unwrap();
        idle(&indexer).await;
        assert_eq!(at(&memory.wallet_index("a")), Some(TIP));
        assert_eq!(at(&memory.wallet_index("b")), Some(9));
        set(IndexState::Indexing).await.unwrap();
        idle(&indexer).await;
        // The store moves an index only onto the child of its block.
        assert_eq!(at(&memory.wallet_index("b")), Some(TIP));
    }
}
