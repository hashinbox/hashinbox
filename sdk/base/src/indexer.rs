//! [`Indexer`]: one network's chain into its [`BlockStore`], a step at a time.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::ensure;

use crate::{
    Addressed, Asset, BlockHeader, BlockStore, Chained, Network, Networked, WalletIndex,
    WalletIndexStore,
};

/// Blocks fetched per step unless [`Indexer::with_batch`] says otherwise.
pub const DEFAULT_BATCH: u64 = 100;

/// What one [`Indexer::next`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Kept blocks through this one.
    Indexed(BlockHeader),
    /// Archived kept blocks from this position: the chain took them back.
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
    /// Kept blocks archived because the chain took them back.
    pub reorgs: u64,
    /// Addresses watched.
    pub watched: u64,
    /// Failed steps.
    pub failures: u64,
    /// The last failure, cleared by the next kept block.
    pub error: Option<String>,
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
/// archiving kept blocks the chain took back. The caller steps it
/// ([`Indexer::next`]), about every [`Indexer::interval`].
pub struct Indexer {
    chain: Arc<dyn Chained>,
    blocks: Arc<dyn BlockStore>,
    indexes: Arc<dyn WalletIndexStore>,
    interval: Duration,
    batch: u64,
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

    /// Indexes `asset` of `address` from `birthday`; [`Indexer::next`]
    /// backfills it.
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
        let birthday = match birthday {
            Birthday::Fresh => self.chain.tips().await?.produced,
            Birthday::At(position) => position,
        };
        let index = WalletIndex {
            address: address.clone(),
            asset: asset.clone(),
            birthday,
            indexed_at: None,
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

    /// One step of at most a batch: from the furthest-behind index, or else
    /// from the last kept block. When the chain no longer has
    /// the last kept block, archives it; the next step checks the one below.
    ///
    /// # Errors
    ///
    /// When the chain or a store fails, or the chain changes mid-fetch
    /// (retry); counted in [`Stats::failures`].
    pub async fn next(&self) -> anyhow::Result<Next> {
        let next = self.step().await;
        if let Err(error) = &next {
            let mut stats = self.stats.get();
            stats.failures += 1;
            stats.error = Some(format!("{error:#}"));
        }
        next
    }

    async fn step(&self) -> anyhow::Result<Next> {
        let network = self.network();
        let last = self.blocks.find_last(network).await?;
        let lagging = self
            .indexes
            .find_next(network, last.as_ref().map(|last| last.position))
            .await?;
        // Forward starts at the last kept block, to see it is still on the chain.
        let Some(start) = lagging.or(last.as_ref().map(|last| last.position)) else {
            return Ok(Next::Idle);
        };
        let tip = self.chain.tips().await?.produced;
        self.stats.get().tip = Some(tip);
        let end = tip.min(start.saturating_add(self.batch - 1));
        let run = self.chain.blocks(start, end).await?;
        // The run may end early (the node has no more yet): only the
        // positions it reached can tell whether the last kept block is gone.
        let Some(reached) = run.last().map(|header| header.position) else {
            return Ok(Next::Idle);
        };

        if let Some(last) = &last
            && last.position <= reached
            && !run.contains(last)
        {
            self.blocks.delete(network, last.position).await?;
            self.stats.get().reorgs += 1;
            return Ok(Next::Reorged(last.position));
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
}

impl Networked for Indexer {
    fn network(&self) -> &Network {
        self.chain.network()
    }
}
