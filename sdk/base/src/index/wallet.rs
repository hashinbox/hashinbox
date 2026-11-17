//! A wallet's index: one asset of one address ([`WalletIndex`]), what a
//! caller asks to watch ([`WalletIndexTarget`]), where indexes are kept
//! ([`WalletIndexStore`]), and the part of the indexer that closes their
//! gaps ([`WalletIndexer`]).

use std::sync::Arc;

use anyhow::{Result, ensure};

use super::{CLOSES, IndexState, Indexed, IntoIndex, Progress, Skipped, behind, lowest_next};
use crate::{
    Address, Addressed, Asset, Balances, BlockHeader, Chained, Future, Holding, Indexes, Line,
    Network, Networked, NotServed, Tips, WalletError, WalletErrorKind,
};

/// Where a new watch starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Birthday {
    /// At the chain's produced tip: no history.
    Fresh,
    /// At this position: history from there on.
    At(u64),
}

/// How far one asset of one address has been indexed. Progress is per
/// asset, so an asset added later backfills from its own birthday without
/// disturbing the address's other assets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletIndex {
    /// The indexed address; keyed or watch-only.
    pub address: Address,
    /// The indexed asset, of the address's network.
    pub asset: Asset,
    /// The start asked for.
    pub requested_from: u64,
    /// The first block it reads: `requested_from`, the first block the node
    /// served when it no longer served that one, or where a jump or a
    /// settle moved it (on a chain whose line is [`Line::Final`], a watch
    /// starts at the chain's final block).
    pub birthday: u64,
    /// How far it has read.
    pub progress: Progress,
}

impl Indexed for WalletIndex {
    fn start(&self) -> u64 {
        self.birthday
    }

    fn progress(&self) -> &Progress {
        &self.progress
    }
}

/// A finished index is its own target.
impl IntoIndex<Self> for WalletIndex {
    fn index(&self) -> Future<'_, Result<Self>> {
        let index = self.clone();
        Box::pin(async move { Ok(index) })
    }
}

impl Addressed for WalletIndex {
    fn address(&self) -> &Address {
        &self.address
    }
}

impl Networked for WalletIndex {
    fn network(&self) -> &Network {
        &self.address.network
    }
}

/// An asset of an address a caller wants indexed, from a start; the chain
/// fills in the rest ([`IntoIndex`]).
pub struct WalletIndexTarget {
    chain: Arc<dyn Chained>,
    address: Address,
    asset: Asset,
    start: Birthday,
}

impl WalletIndexTarget {
    /// `asset` of `address`, on `chain`, from its tip ([`Birthday::Fresh`]).
    #[must_use]
    pub fn new(chain: Arc<dyn Chained>, address: Address, asset: Asset) -> Self {
        Self {
            chain,
            address,
            asset,
            start: Birthday::Fresh,
        }
    }

    /// From `start` instead.
    #[must_use]
    pub const fn with_birthday(mut self, start: Birthday) -> Self {
        self.start = start;
        self
    }
}

/// The tips; `requested_from` the start (the produced tip when fresh); the
/// birthday by the chain's line: [`Line::Final`], its final block (it
/// settles there next step); [`Line::Depth`], the start, refused when older
/// than the tip less the depth.
impl IntoIndex<WalletIndex> for WalletIndexTarget {
    fn index(&self) -> Future<'_, Result<WalletIndex>> {
        Box::pin(async move {
            let network = self.chain.network();
            ensure!(
                self.address.network == *network && self.asset.network() == network,
                "{:?} or {:?} is not of {network:?}",
                self.address.network,
                self.asset
            );
            let tips = self.chain.tips().await?;
            let requested_from = match self.start {
                Birthday::Fresh => tips.produced,
                Birthday::At(position) => position,
            };
            let birthday = match self.chain.line() {
                Line::Final => tips.confirmed,
                line @ Line::Depth(depth) => {
                    let earliest = line.at(tips);
                    ensure!(
                        requested_from >= earliest,
                        WalletError::new(
                            WalletErrorKind::InvalidStart,
                            format!(
                                "start {requested_from} is more than this network's max_depth \
                                 of {depth} positions below the tip: start at {earliest} or later"
                            ),
                        )
                    );
                    requested_from
                }
            };
            Ok(WalletIndex {
                address: self.address.clone(),
                asset: self.asset.clone(),
                requested_from,
                birthday,
                progress: Progress::default(),
            })
        })
    }
}

/// Where wallet indexes are kept.
pub trait WalletIndexStore: Send + Sync {
    /// Keeps the index `index` becomes; one already kept keeps its birthday
    /// and progress.
    ///
    /// # Errors
    ///
    /// When the chain refuses the target, or the store cannot write.
    fn insert<'a>(&'a self, index: &'a dyn IntoIndex<WalletIndex>) -> Future<'a, Result<()>>;

    /// Every index of `network`, whatever its state.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_all<'a>(&'a self, network: &'a Network) -> Future<'a, Result<Vec<WalletIndex>>>;

    /// The lowest [`Indexed::next`] of `network`'s [`IndexState::Indexing`]
    /// indexes at or above `from` and at or below `through`; with `None`,
    /// at or above `from`. `None` when no index qualifies. By default
    /// from [`Self::find_all`].
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_next<'a>(
        &'a self,
        network: &'a Network,
        from: u64,
        through: Option<u64>,
    ) -> Future<'a, Result<Option<u64>>> {
        Box::pin(async move { Ok(lowest_next(&self.find_all(network).await?, from, through)) })
    }

    /// `network`'s [`IndexState::Indexing`] indexes whose [`Indexed::next`]
    /// is below `below`, furthest behind first, at most `limit`. By default
    /// from [`Self::find_all`].
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_behind<'a>(
        &'a self,
        network: &'a Network,
        below: u64,
        limit: usize,
    ) -> Future<'a, Result<Vec<WalletIndex>>> {
        Box::pin(async move { Ok(behind(self.find_all(network).await?, below, limit)) })
    }

    /// Moves the birthday of `index` to `start`, only while it has not
    /// started (`indexed_at` unset); its `requested_from` stays.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn move_start<'a>(&'a self, index: &'a WalletIndex, start: u64) -> Future<'a, Result<()>>;

    /// Sets the state of `index`, and its reason (`None` clears it).
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn set_state<'a>(
        &'a self,
        index: &'a WalletIndex,
        state: IndexState,
        reason: Option<&'a str>,
    ) -> Future<'a, Result<()>>;

    /// Settles `index` at `holding`, what the chain says it holds at a
    /// final block `S` (`holding.position`, its header `header`), in one
    /// write: `S` kept unless it is; on it, an `adjustment` of
    /// `holding.amount` less the index's total (its live legs, none above
    /// `S`), even a zero one (it marks the opening); the index continuing
    /// from `S + 1`. Its pending sends stay: each one's transaction index
    /// settles itself. `false`: another live block at `S`, or `S` already
    /// holds its adjustment; nothing written; try again later.
    ///
    /// # Errors
    ///
    /// When the store cannot read or write, or `S` does not link onto the
    /// kept blocks around it.
    fn settle<'a>(
        &'a self,
        index: &'a WalletIndex,
        holding: &'a Holding,
        header: &'a BlockHeader,
    ) -> Future<'a, Result<bool>>;

    /// Restarts `index` at `birthday`, the jump line it fell below, in one
    /// write: its birthday there, `indexed_at` unset, indexing; its
    /// `requested_from` and legs stay. Its pending sends are forgotten: one
    /// mined below `birthday` is never read. A store keeping coins archives
    /// those `index` still shows unspent and writes them off on its
    /// `indexed_at`.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn restart<'a>(&'a self, index: &'a WalletIndex, birthday: u64) -> Future<'a, Result<()>>;
}

/// The wallet indexes of one network, as one part of its
/// [`Indexer`](crate::Indexer): their gaps below the line closed by the
/// chain's balance ([`Balances`]: a settle) or, without one, by a jump.
pub struct WalletIndexer {
    store: Arc<dyn WalletIndexStore>,
    chain: Arc<dyn Chained>,
    balances: Option<Arc<dyn Balances>>,
}

impl WalletIndexer {
    /// The indexes of `store` on `chain`, settled through `balances` when
    /// the chain reads them.
    #[must_use]
    pub fn new(
        store: Arc<dyn WalletIndexStore>,
        chain: Arc<dyn Chained>,
        balances: Option<Arc<dyn Balances>>,
    ) -> Self {
        Self {
            store,
            chain,
            balances,
        }
    }

    /// Closes `index`'s gap. With balances, settles it at a final block at
    /// or above `at`: reads its balance there (`S`) and, once the chain
    /// answers and serves `S`, writes it ([`WalletIndexStore::settle`]); it
    /// reads on from `S + 1`. Not answerable yet: it stays as it is for the
    /// next step. Without (Bitcoin), restarts it at `line`
    /// ([`WalletIndexStore::restart`]): the gap below is never read.
    async fn close_one(&self, index: &WalletIndex, line: u64, at: u64) -> Result<()> {
        let Some(balances) = &self.balances else {
            return self.store.restart(index, line).await;
        };
        let read = balances.balance(&index.address, &index.asset, at);
        let Some(holding) = read.await? else {
            return Ok(());
        };
        let Some(block) = self.chain.block(holding.position).await? else {
            return Ok(());
        };
        self.store.settle(index, &holding, &block.header).await?;
        Ok(())
    }
}

impl Networked for WalletIndexer {
    fn network(&self) -> &Network {
        self.chain.network()
    }
}

impl Indexes for WalletIndexer {
    fn next<'a>(&'a self, from: u64, through: Option<u64>) -> Future<'a, Result<Option<u64>>> {
        self.store.find_next(self.network(), from, through)
    }

    /// Closes the gaps of at most a few indexes whose next block is below
    /// `line` ([`Self::close_one`]), with `tips.confirmed` the final block;
    /// one failing does not stop the others, and the first failure is the
    /// answer.
    fn close<'a>(&'a self, line: u64, tips: Tips) -> Future<'a, Result<()>> {
        Box::pin(async move {
            let behind = self.store.find_behind(self.network(), line, CLOSES);
            let mut failed = None;
            for index in &behind.await? {
                if let Err(error) = self.close_one(index, line, tips.confirmed).await {
                    failed.get_or_insert(error);
                }
            }
            failed.map_or(Ok(()), Err)
        })
    }

    fn skip<'a>(&'a self, below: u64, start: u64) -> Future<'a, Result<Skipped>> {
        Box::pin(async move {
            let mut skipped = Skipped::default();
            let behind = self.store.find_behind(self.network(), below, usize::MAX);
            for index in &behind.await? {
                let Some(position) = index.next() else {
                    continue;
                };
                if index.progress.indexed_at.is_none() {
                    self.store.move_start(index, start).await?;
                    skipped.moved = true;
                } else {
                    let reason = NotServed { position }.to_string();
                    let state = IndexState::NoData;
                    self.store.set_state(index, state, Some(&reason)).await?;
                    skipped.stopped = true;
                }
            }
            Ok(skipped)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Chain};

    /// A chain at tip 100, final through 90, with line `.0`.
    struct Lined(Line, Network);

    impl Networked for Lined {
        fn network(&self) -> &Network {
            &self.1
        }
    }

    impl Chained for Lined {
        fn tips(&self) -> Future<'_, Result<Tips>> {
            Box::pin(async {
                Ok(Tips {
                    produced: 100,
                    confirmed: 90,
                })
            })
        }

        fn block(&self, _position: u64) -> Future<'_, Result<Option<Block>>> {
            Box::pin(async { Ok(None) })
        }

        fn line(&self) -> Line {
            self.0
        }
    }

    /// What alice's native index from `start` becomes on a chain with `line`.
    async fn index(line: Line, start: Birthday) -> Result<WalletIndex> {
        let network = Network::new(Chain::from_static("test"), "lined");
        let (address, asset) = (
            Address::new(network.clone(), "alice"),
            Asset::native(network.clone()),
        );
        let target = WalletIndexTarget::new(Arc::new(Lined(line, network)), address, asset);
        target.with_birthday(start).index().await
    }

    #[tokio::test]
    async fn a_start_older_than_the_depth_line_is_refused() {
        let error = index(Line::Depth(30), Birthday::At(69)).await.unwrap_err();
        let refused = error.downcast_ref::<WalletError>().unwrap();
        assert_eq!(refused.kind, WalletErrorKind::InvalidStart);
        assert!(refused.message.contains("start at 70"), "{refused}");
        for (start, from) in [(Birthday::At(70), 70), (Birthday::Fresh, 100)] {
            let index = index(Line::Depth(30), start).await.unwrap();
            assert_eq!((index.requested_from, index.birthday), (from, from));
            assert_eq!(index.progress, Progress::default());
        }
    }

    #[tokio::test]
    async fn a_final_line_starts_at_the_final_block_whatever_the_start() {
        for (start, from) in [(Birthday::At(2), 2), (Birthday::Fresh, 100)] {
            let index = index(Line::Final, start).await.unwrap();
            assert_eq!((index.requested_from, index.birthday), (from, 90));
        }
    }
}
