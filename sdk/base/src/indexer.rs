//! [`Indexer`]: one network's chain into its [`BlockStore`], a step at a time.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use logging::{Level, Logger};
use tokio::task::JoinHandle;

use anyhow::{Context as _, ensure};

use crate::{
    Addressed, BlockHeader, BlockStore, Chained, Network, Networked, Notified, WalletIndex,
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

/// Indexes one network: fetches blocks up to the produced tip and keeps them,
/// archiving kept blocks the chain took back.
pub struct Indexer {
    chain: Arc<dyn Chained>,
    blocks: Arc<dyn BlockStore>,
    indexes: Arc<dyn WalletIndexStore>,
    notified: Arc<dyn Notified>,
    batch: u64,
    logger: Logger,
    stats: Mutex<Stats>,
}

impl Indexer {
    /// Indexes `chain` into `blocks`, for the addresses of `indexes`, woken
    /// by `notified`.
    #[must_use]
    pub fn new(
        chain: Arc<dyn Chained>,
        blocks: Arc<dyn BlockStore>,
        indexes: Arc<dyn WalletIndexStore>,
        notified: Arc<dyn Notified>,
    ) -> Self {
        Self {
            chain,
            blocks,
            indexes,
            notified,
            batch: DEFAULT_BATCH,
            logger: Logger::default(),
            stats: Mutex::default(),
        }
    }

    /// Logs each step of [`Indexer::run`] to `logger`; none by default.
    #[must_use]
    pub fn with_logger(mut self, logger: Logger) -> Self {
        self.logger = logger;
        self
    }

    /// Fetches at most `batch` positions per step (at least one).
    #[must_use]
    pub fn with_batch(mut self, batch: u64) -> Self {
        self.batch = batch.max(1);
        self
    }

    /// Indexes `address` from `birthday`; [`Indexer::next`] backfills it.
    ///
    /// # Errors
    ///
    /// When `address` is of another network, or the chain or store fails.
    pub async fn watch(&self, address: &dyn Addressed, birthday: Birthday) -> anyhow::Result<()> {
        let address = address.address();
        ensure!(
            address.network == *self.network(),
            "{:?} is not of {:?}",
            address.network,
            self.network()
        );
        let birthday = match birthday {
            Birthday::Fresh => self.chain.tips().await?.produced,
            Birthday::At(position) => position,
        };
        let index = WalletIndex {
            address: address.clone(),
            birthday,
            indexed_at: None,
        };
        self.indexes.insert(&index).await?;
        self.record(|stats| stats.watched += 1);
        Ok(())
    }

    /// What it did so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, change: impl FnOnce(&mut Stats)) {
        change(&mut self.stats.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// One step of at most a batch: from the furthest-behind index, or else
    /// from the last kept block. When the chain no longer has
    /// the last kept block, archives it; the next step checks the one below.
    ///
    /// # Errors
    ///
    /// When the chain or a store fails, or the chain changes mid-fetch
    /// (retry).
    pub async fn next(&self) -> anyhow::Result<Next> {
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
        self.record(|stats| stats.tip = Some(tip));
        let end = tip.min(start.saturating_add(self.batch - 1));
        let run = self.chain.blocks(start, end).await?;

        if let Some(last) = &last
            && last.position <= end
            && !run.contains(last)
        {
            self.blocks.delete(network, last.position).await?;
            self.record(|stats| stats.reorgs += 1);
            return Ok(Next::Reorged(last.position));
        }
        let Some(header) = run.last().cloned() else {
            return Ok(Next::Idle);
        };
        if lagging.is_none() && last.as_ref() == Some(&header) {
            return Ok(Next::Idle);
        }
        self.blocks.insert(&run).await?;
        self.record(|stats| {
            stats.blocks += run.len() as u64;
            stats.last = Some(header.clone());
            stats.error = None;
        });
        Ok(Next::Indexed(header))
    }

    /// Steps until [`Next::Idle`] or a failure, logging each step.
    pub async fn drain(&self) {
        let logger = self.logger.with("network", format!("{:?}", self.network()));
        loop {
            let (level, next) = match self.next().await {
                Ok(Next::Idle) => return,
                Ok(next @ Next::Indexed(_)) => (Level::Debug, format!("{next:?}")),
                Ok(next @ Next::Reorged(_)) => (Level::Warn, format!("{next:?}")),
                Err(error) => {
                    let error = format!("{error:#}");
                    self.record(|stats| {
                        stats.failures += 1;
                        stats.error = Some(error.clone());
                    });
                    (Level::Error, error)
                }
            };
            logger.event(level, "indexer.next", &next, &[]);
            if level == Level::Error {
                return;
            }
        }
    }

    /// Runs on its own task until stopped: [`Indexer::drain`] on every
    /// notification; the task ends when the notifications close.
    #[must_use]
    pub fn run(self: Arc<Self>) -> Handle {
        let mut notifications = self.notified.subscribe();
        Handle(tokio::spawn(async move {
            while notifications.recv().await.is_some() {
                self.drain().await;
            }
        }))
    }
}

/// A running [`Indexer::run`].
pub struct Handle(JoinHandle<()>);

impl Handle {
    /// Stops the run at its next await; every write it made is whole.
    pub fn stop(&self) {
        self.0.abort();
    }

    /// Waits for the run to end.
    ///
    /// # Errors
    ///
    /// When the run panicked.
    pub async fn join(self) -> anyhow::Result<()> {
        match self.0.await {
            Err(error) if error.is_panic() => Err(error.into()),
            _ => Ok(()),
        }
    }
}

/// Several runs, stopped and joined together.
#[derive(Default)]
pub struct Handles(Vec<Handle>);

impl Handles {
    /// Adds `handle`.
    pub fn push(&mut self, handle: Handle) {
        self.0.push(handle);
    }

    /// Asks every run to stop.
    pub fn stop(&self) {
        self.0.iter().for_each(Handle::stop);
    }

    /// Waits for every run to end.
    ///
    /// # Errors
    ///
    /// The first run that panicked; the rest are still awaited.
    pub async fn join(self) -> anyhow::Result<()> {
        let mut result = Ok(());
        for handle in self.0 {
            let joined = handle.join().await;
            if result.is_ok() {
                result = joined;
            }
        }
        result
    }
}

/// One [`Indexer`] per network.
#[derive(Clone, Default)]
pub struct Indexers(BTreeMap<Network, Arc<Indexer>>);

impl Indexers {
    /// Adds `indexer` under its network.
    ///
    /// # Errors
    ///
    /// When the network has one already.
    pub fn insert(&mut self, indexer: Indexer) -> anyhow::Result<()> {
        let network = indexer.network().clone();
        ensure!(
            !self.0.contains_key(&network),
            "{network:?} has an indexer already"
        );
        self.0.insert(network, Arc::new(indexer));
        Ok(())
    }

    /// The indexer of `network`.
    ///
    /// # Errors
    ///
    /// When the network has none.
    pub fn get(&self, network: &Network) -> anyhow::Result<&Arc<Indexer>> {
        self.0
            .get(network)
            .with_context(|| format!("{network:?} has no indexer"))
    }

    /// Indexes `address` from `birthday`, on its network's indexer.
    ///
    /// # Errors
    ///
    /// When its network has no indexer, or [`Indexer::watch`] fails.
    pub async fn watch(&self, address: &dyn Addressed, birthday: Birthday) -> anyhow::Result<()> {
        self.get(address.network())?.watch(address, birthday).await
    }

    /// Every network's stats.
    #[must_use]
    pub fn stats(&self) -> BTreeMap<Network, Stats> {
        self.0
            .iter()
            .map(|(network, indexer)| (network.clone(), indexer.stats()))
            .collect()
    }

    /// Runs every indexer, each on its own task.
    #[must_use]
    pub fn run(&self) -> Handles {
        Handles(
            self.0
                .values()
                .map(|indexer| Arc::clone(indexer).run())
                .collect(),
        )
    }
}

impl Networked for Indexer {
    fn network(&self) -> &Network {
        self.chain.network()
    }
}
