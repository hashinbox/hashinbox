use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::Context as _;

use crate::{
    Address, Addressed, Asset, Decimal, Future, Network, Networked, transaction::Transactions,
};

/// Canonical block identity in the chain's native byte order.
///
/// The base layer deliberately does not impose a fixed length: common chains
/// use 32 bytes, while future protocols may use a different digest format.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub Vec<u8>);

/// Atomic identity of one canonical block's parent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Parent {
    /// The parent's native coordinate.
    pub position: u64,
    /// The parent's hash, compared on the next block to detect a reorg.
    pub hash: Hash,
}

impl From<(u64, Hash)> for Parent {
    fn from((position, hash): (u64, Hash)) -> Self {
        Self { position, hash }
    }
}

impl From<Parent> for (u64, Hash) {
    fn from(parent: Parent) -> Self {
        (parent.position, parent.hash)
    }
}

/// Chain-independent header required for canonical traversal and reorg
/// detection. Protocol-specific block fields stay in concrete chain crates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Native coordinate (Bitcoin/Ethereum height, Solana slot).
    pub position: u64,
    /// Produced-block count, used for confirmations.
    pub height: u64,
    /// The block's own hash.
    pub hash: Hash,
    /// The parent block, or `None` for genesis.
    pub parent: Option<Parent>,
    /// Unix seconds, when the chain reports one.
    pub timestamp: Option<u64>,
}

impl Header {
    /// Whether this block directly follows `previous`: its position is above
    /// (positions may be sparse), its height is one more, and its parent is
    /// exactly `previous`.
    #[must_use]
    pub fn follows(&self, previous: &Self) -> bool {
        previous.height.checked_add(1) == Some(self.height)
            && self.position > previous.position
            && self.parent.as_ref().is_some_and(|parent| {
                parent.position == previous.position && parent.hash == previous.hash
            })
    }
}

/// One block of one network: its transactions and its rewards.
///
/// The network is stated by whoever builds the block: a [`Header`] is the
/// same bytes on every network that shares a history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The network it was read from.
    pub network: Network,
    /// Its identity and parent link.
    pub header: Header,
    /// Its mined transactions, in block order.
    pub transactions: Transactions,
    /// Value the block itself credited or debited.
    pub rewards: Vec<Reward>,
}

impl Block {
    /// Whether this block directly follows `previous` ([`Header::follows`]).
    #[must_use]
    pub fn follows(&self, previous: &Self) -> bool {
        self.network == previous.network && self.header.follows(&previous.header)
    }

    /// Every address a leg or reward names.
    #[must_use]
    pub fn addresses(&self) -> BTreeSet<&Address> {
        let mut addresses = self.transactions.addresses();
        addresses.extend(self.rewards.iter().map(Addressed::address));
        addresses
    }

    /// Whether it changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty() && self.rewards.is_empty()
    }
}

impl Networked for Block {
    fn network(&self) -> &Network {
        &self.network
    }
}

/// A linked run of blocks of one network, ascending: each follows the one
/// before it ([`Header::follows`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocks {
    network: Network,
    blocks: Vec<Block>,
}

impl Blocks {
    /// No blocks yet, of `network`.
    #[must_use]
    pub const fn new(network: Network) -> Self {
        Self {
            network,
            blocks: Vec::new(),
        }
    }

    /// Appends `block`; `false` (and nothing appended) when it is of another
    /// network or does not follow the last.
    #[must_use]
    pub fn push(&mut self, block: Block) -> bool {
        let linked = match self.blocks.last() {
            Some(last) => block.follows(last),
            None => block.network == self.network,
        };
        if linked {
            self.blocks.push(block);
        }
        linked
    }

    /// The last block's header.
    #[must_use]
    pub fn last(&self) -> Option<&Header> {
        self.blocks.last().map(|block| &block.header)
    }

    /// Whether it holds the block of `header`.
    #[must_use]
    pub fn contains(&self, header: &Header) -> bool {
        self.blocks.iter().any(|block| &block.header == header)
    }

    /// Drops every block at or above `position`.
    pub fn truncate(&mut self, position: u64) {
        self.blocks.retain(|block| block.header.position < position);
    }

    /// Removes and returns every block at or below `position`, in order.
    #[must_use]
    pub fn split(&mut self, position: u64) -> Self {
        let at = self
            .blocks
            .partition_point(|block| block.header.position <= position);
        let rest = self.blocks.split_off(at);
        let taken = std::mem::replace(&mut self.blocks, rest);
        Self {
            network: self.network.clone(),
            blocks: taken,
        }
    }

    /// Every address a leg or reward of any block names.
    #[must_use]
    pub fn addresses(&self) -> BTreeSet<&Address> {
        self.blocks.iter().flat_map(Block::addresses).collect()
    }
}

impl Networked for Blocks {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl std::ops::Deref for Blocks {
    type Target = [Block];

    fn deref(&self) -> &[Block] {
        &self.blocks
    }
}

/// Value the block itself moved, with no transaction (withdrawal, staking or
/// block reward). Signed: Solana rent can debit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reward {
    /// Its position in the block's reward list (Ethereum: the withdrawal index).
    pub index: u64,
    /// The address credited or debited.
    pub address: Address,
    /// The asset.
    pub asset: Asset,
    /// Signed atomic amount.
    pub amount: Decimal,
}

impl Networked for Reward {
    fn network(&self) -> &Network {
        &self.address.network
    }
}

impl Addressed for Reward {
    fn address(&self) -> &Address {
        &self.address
    }
}

/// A chain's newest block: its position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tips {
    /// The newest block's position.
    pub position: u64,
}

impl Tips {
    /// The confirmed position: `confirmations` (the network's config) below
    /// the tip, saturating. The one way confirmed is computed.
    #[must_use]
    pub const fn confirmed(self, confirmations: u64) -> u64 {
        self.position.saturating_sub(confirmations)
    }
}

/// Where a chain's reading starts and an index's gaps close: its own rule,
/// from its config ([`Chained::line`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    /// Ethereum: the block after the confirmed one ([`Tips::confirmed`]).
    Final,
    /// The tip less this many positions: Bitcoin's `max_depth` (`u64::MAX`:
    /// no limit), Solana's `read_depth`.
    Depth(u64),
}

impl Line {
    /// The position nothing below is read, at `tips` with `confirmations`
    /// ([`Chained::confirmations`]).
    #[must_use]
    pub const fn at(self, tips: Tips, confirmations: u64) -> u64 {
        match self {
            Self::Final => tips.confirmed(confirmations).saturating_add(1),
            Self::Depth(depth) => tips.position.saturating_sub(depth),
        }
    }
}

/// The node no longer serves the block at `position`: its history there is
/// pruned. A chain fails a read with it, inside an [`anyhow::Error`];
/// [`NotServed::find`] finds it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotServed {
    /// The block's native coordinate.
    pub position: u64,
}

impl NotServed {
    /// The one `error` carries, anywhere in its chain of causes.
    #[must_use]
    pub fn find(error: &anyhow::Error) -> Option<Self> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<Self>())
            .copied()
    }
}

impl std::fmt::Display for NotServed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "node no longer serves block {}", self.position)
    }
}

impl std::error::Error for NotServed {}

/// A chain that can be read block by block.
pub trait Chained: Networked + Send + Sync {
    /// The newest block ([`Tips`]).
    ///
    /// # Errors
    ///
    /// When the node cannot answer.
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>>;

    /// Positions a block needs below the tip to count as confirmed (the
    /// network's config): [`Tips::confirmed`].
    fn confirmations(&self) -> u64;

    /// The block at `position`, fully interpreted; `None` where no block was
    /// produced (a skipped Solana slot).
    ///
    /// # Errors
    ///
    /// When the node cannot answer or the block cannot be read.
    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>>;

    /// Where reading starts and gaps close. Default: no line,
    /// `Depth(u64::MAX)`: every block may be read.
    fn line(&self) -> Line {
        Line::Depth(u64::MAX)
    }

    /// Blocks `start..=end`, linked, skipped positions omitted. It may end
    /// early, before the first block the node cannot fully serve yet (a
    /// node behind a load balancer); that is no error, the rest comes on a
    /// later call. Defaults to [`Chained::block`] one at a time; a chain may
    /// fetch them together.
    ///
    /// # Errors
    ///
    /// When a block cannot be read, or one does not link to the one before
    /// it (the chain changed mid-fetch; retry).
    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(async move {
            let network = self.network();
            let mut run = Blocks::new(network.clone());
            for position in start..=end {
                let block = self
                    .block(position)
                    .await
                    .with_context(|| format!("block {position}"))?;
                if let Some(block) = block
                    && !run.push(block)
                {
                    anyhow::bail!("{network:?}: block {position} does not link; retry");
                }
            }
            Ok(run)
        })
    }
}

/// How many positions [`Dated::position_at`] steps over looking for a
/// produced block (a run of skipped Solana slots) before it gives up.
const SKIP_LIMIT: u64 = 1_000;

/// A produced block's position and time: what [`Dated`] finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Native coordinate (Bitcoin/Ethereum height, Solana slot).
    pub position: u64,
    /// Unix seconds.
    pub timestamp: u64,
}

/// A chain that can find its blocks by time: optional, a
/// [`Service`](crate::Service) may lack it. The search reads only
/// [`Dated::time`], which defaults to a whole block read through
/// [`Chained::block`]; a chain with a lighter header read overrides it.
pub trait Dated: Chained {
    /// The time (Unix seconds) of the block at `position`; `None` where no
    /// block was produced (a skipped Solana slot).
    ///
    /// # Errors
    ///
    /// When the node cannot answer, or the block carries no time.
    fn time(&self, position: u64) -> Future<'_, anyhow::Result<Option<u64>>> {
        Box::pin(async move {
            let Some(block) = self.block(position).await? else {
                return Ok(None);
            };
            let time = block
                .header
                .timestamp
                .with_context(|| format!("block {position} carries no time"))?;
            Ok(Some(time))
        })
    }

    /// The newest produced block.
    ///
    /// # Errors
    ///
    /// When the node cannot answer, or holds no block at its own tip.
    fn tip(&self) -> Future<'_, anyhow::Result<Stamp>> {
        Box::pin(async move {
            let position = self.tips().await?.position;
            let timestamp = self
                .time(position)
                .await?
                .with_context(|| format!("no block at the tip {position}"))?;
            Ok(Stamp {
                position,
                timestamp,
            })
        })
    }

    /// The last block at or before `time` (Unix seconds); `None` when `time`
    /// is before the chain's first block: [`Dated::search`] without progress.
    ///
    /// # Errors
    ///
    /// As [`Dated::search`].
    fn position_at(&self, time: u64) -> Future<'_, anyhow::Result<Option<Stamp>>> {
        Box::pin(async move { self.search(time, &|_| {}).await })
    }

    /// The last block at or before `time` (Unix seconds); `None` when `time`
    /// is before the chain's first block. `progress` sees the range the
    /// answer is in: the chain's first block and its tip, then after each
    /// step, narrower or the same.
    ///
    /// A binary search over positions `0..=tip`: about `log2(tip)`
    /// [`Dated::time`] reads. Positions without a block are stepped over.
    /// Block times only roughly ascend on Bitcoin, so there the answer may be
    /// a few blocks off.
    ///
    /// # Errors
    ///
    /// When the node cannot answer, a block carries no time, or more than
    /// a thousand positions in a row hold no block.
    fn search<'a>(
        &'a self,
        time: u64,
        progress: &'a (dyn Fn(Probe) + Send + Sync),
    ) -> Future<'a, anyhow::Result<Option<Stamp>>> {
        Box::pin(async move {
            let tip = self.tip().await?;
            if tip.timestamp <= time {
                return Ok(Some(tip));
            }
            let (start, time_there) = first_timed(self, tip.position).await?;
            let first = match time_there {
                Some(timestamp) => Some(Stamp {
                    position: start,
                    timestamp,
                }),
                None => produced(self, start, tip.position).await?,
            };
            let Some(first) = first else {
                return Ok(None);
            };
            if first.timestamp > time {
                return Ok(None);
            }
            // `low` is at or before `time`, `high` after it; no position from
            // `end` on holds a block at or before it.
            let (mut low, mut high, mut end) = (first, tip, tip.position);
            progress(Probe { low, high });
            while end - low.position > 1 {
                let middle = low.position + (end - low.position) / 2;
                match produced(self, middle, end - 1).await? {
                    None => end = middle,
                    Some(stamp) if stamp.timestamp <= time => low = stamp,
                    Some(stamp) => (high, end) = (stamp, stamp.position),
                }
                progress(Probe { low, high });
            }
            Ok(Some(low))
        })
    }
}

/// One step of [`Dated::search`]: the block it looks for is at or after
/// `low` and before `high`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    /// A produced block at or before the time asked for.
    pub low: Stamp,
    /// A produced block after it.
    pub high: Stamp,
}

/// The first produced block in `start..=end`, stepping over at most
/// [`SKIP_LIMIT`] empty positions.
/// The first position whose time the node serves (0, or the first past
/// what it pruned), with the time read there when that read was made
/// (`None`: no block there, or not read). Pruning removes a prefix, so
/// "served" flips once; the tip is taken as served.
///
/// # Errors
///
/// A failed read other than [`NotServed`].
async fn first_timed<C: Dated + ?Sized>(chain: &C, tip: u64) -> anyhow::Result<(u64, Option<u64>)> {
    /// `None`: not served; else the time there (`None`: no block).
    async fn read<C: Dated + ?Sized>(
        chain: &C,
        position: u64,
    ) -> anyhow::Result<Option<Option<u64>>> {
        match chain.time(position).await {
            Ok(time) => Ok(Some(time)),
            Err(error) if NotServed::find(&error).is_some() => Ok(None),
            Err(error) => Err(error),
        }
    }
    if let Some(time) = read(chain, 0).await? {
        return Ok((0, time));
    }
    // `low` is not served, `high` is.
    let (mut low, mut high, mut time_high) = (0, tip, None);
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        match read(chain, middle).await? {
            Some(time) => (high, time_high) = (middle, time),
            None => low = middle,
        }
    }
    Ok((high, time_high))
}

async fn produced<C: Dated + ?Sized>(
    chain: &C,
    start: u64,
    end: u64,
) -> anyhow::Result<Option<Stamp>> {
    for position in start..=end {
        anyhow::ensure!(
            position - start < SKIP_LIMIT,
            "no block in {SKIP_LIMIT} positions from {start}"
        );
        if let Some(timestamp) = chain.time(position).await? {
            return Ok(Some(Stamp {
                position,
                timestamp,
            }));
        }
    }
    Ok(None)
}

/// Where indexed blocks are kept: every block the [`Indexer`](crate::Indexer)
/// fetched, with the facts of indexed address assets, for every network.
///
/// # Model
///
/// - A block is kept until [`Store::delete`] takes it back (a reorg): it is
///   deleted, and its facts leave every read: balances, history, unspent
///   outputs.
/// - The kept blocks of a network are one linked run: each names the one
///   below it as its parent. At most one block per position.
/// - Each index, one asset of one address, carries `indexed_at`
///   ([`crate::WalletIndex`]): the address's facts in that asset are kept for
///   every block from its birthday through that position, with no gaps.
///   Progress is per address and asset, so a new address or a newly
///   configured token backfills without disturbing the others.
/// - Blocks are kept up to the produced tip, not only the confirmed one. A
///   confirmed balance is a read over blocks at or below
///   the confirmed position ([`Tips::confirmed`]); a pending balance reads them all. The store records
///   no confirmation itself.
/// - The store never detects a reorg. The indexer compares the last kept
///   block with the chain and calls [`Store::delete`]; the store only refuses
///   a block at a position another kept block holds. Links need no check:
///   a run is linked ([`Blocks`]), and the indexer reads forward from the
///   last kept block.
pub trait Store: Send + Sync {
    /// Keeps `blocks` (one network, linked, as the chain parsed them, every
    /// address unfiltered), in one atomic write that:
    ///
    /// 1. Refuses the whole write when a block conflicts with what is kept: a
    ///    kept block at its position with another hash.
    /// 2. Adds blocks it does not hold; blocks it holds are not added again.
    /// 3. For each block, reads its indexes inside this write and keeps the
    ///    legs, outputs and rewards whose (address, asset) is an index the
    ///    block applies to: birthday at or below it, and `indexed_at` at its
    ///    parent's position or between it and the block's (or unset, when the block is the chain's first at or above
    ///    the birthday: its parent below it); a transaction is kept once,
    ///    when any of its legs is.
    ///    Other facts are dropped.
    /// 4. Marks outputs spent by those legs as spent in that block.
    /// 5. Moves `indexed_at` of those indexes to the block's position, empty
    ///    blocks included, so progress stays contiguous; and of the
    ///    transaction indexes ([`crate::TransactionIndex`]) it applies to by
    ///    the same rule (`broadcasted_at` as the birthday), each noting the
    ///    block's position as `found` (found or reverted, by its outcome)
    ///    when it holds its transaction, which is kept then. Expiry is
    ///    not the indexer's: one not found reads on.
    ///
    /// Indexes added while a step runs are either read by this write or left
    /// for the next step; none is skipped. A repeat of the same blocks applies
    /// to no address and is a no-op, so a retry after an uncertain failure is
    /// harmless.
    ///
    /// # Errors
    ///
    /// When a block conflicts (nothing is written), or the store cannot
    /// write.
    fn insert<'a>(&'a self, blocks: &'a Blocks) -> Future<'a, anyhow::Result<()>>;

    /// The last kept block of `network`; `None` before the first, or when
    /// every block is taken back.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_last<'a>(&'a self, network: &'a Network) -> Future<'a, anyhow::Result<Option<Header>>>;

    /// Deletes every block of `network` at or above `from` (a reorg), in one
    /// atomic write that:
    ///
    /// 1. Turns the transactions a transaction index looks for (sent by the
    ///    caller) mined in them back to pending, their hold on funds kept.
    /// 2. Deletes the blocks with their other transactions, legs, outputs
    ///    and rewards; outputs spent in them are unspent again.
    /// 3. Deletes the adjustments of the network's wallet indexes at or
    ///    above `from`.
    /// 4. Caps `indexed_at` of every index of either kind at `from - 1`
    ///    (unset when that is below the index's start), so those indexes
    ///    read the new branch; a transaction index `found` above it reads
    ///    on.
    ///
    /// Other networks, wallets and indexes stay. Deleting nothing is a no-op.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn delete<'a>(&'a self, network: &'a Network, from: u64) -> Future<'a, anyhow::Result<()>>;
}

/// Tells listeners about each run of blocks a [`Store`] kept.
pub trait Notify: Send + Sync {
    /// Calls `listener` once after each accepted insert, with its run.
    fn subscribe(&self, listener: Box<Listener>);
}

/// A [`Notify`] listener: called with each accepted insert (one network's
/// run of blocks, ascending) after its write.
pub type Listener = dyn Fn(&Blocks) + Send + Sync;

/// A [`Store`] that tells its listeners about each accepted insert, once,
/// after the write commits and outside every lock. A repeated insert is heard
/// again: listeners are hints, never transactional, and cannot veto.
pub struct Notifying<S> {
    inner: S,
    listeners: Mutex<Vec<Arc<Listener>>>,
}

impl<S: Store> Notifying<S> {
    /// Wraps `inner`, with no listeners yet.
    pub const fn new(inner: S) -> Self {
        Self {
            inner,
            listeners: Mutex::new(Vec::new()),
        }
    }

    /// The wrapped store.
    pub const fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: Store> Store for Notifying<S> {
    fn insert<'a>(&'a self, blocks: &'a Blocks) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.inner.insert(blocks).await?;
            let listeners = self
                .listeners
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            listeners.iter().for_each(|listener| listener(blocks));
            Ok(())
        })
    }

    fn find_last<'a>(&'a self, network: &'a Network) -> Future<'a, anyhow::Result<Option<Header>>> {
        self.inner.find_last(network)
    }

    fn delete<'a>(&'a self, network: &'a Network, from: u64) -> Future<'a, anyhow::Result<()>> {
        self.inner.delete(network, from)
    }
}

impl<S: Store> Notify for Notifying<S> {
    fn subscribe(&self, listener: Box<Listener>) {
        self.listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::from(listener));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Chain,
        testing::{Served, fixtures},
    };

    /// Blocks at even positions `0..=20`, block `p` made at time `100 + p`.
    fn even() -> Served {
        let network = Network::new(Chain::from_static("test"), "even");
        let block = |position: u64| {
            let mut block = fixtures::block(&network, position, 0, vec![]);
            block.header.timestamp = Some(100 + position);
            block
        };
        Served::new((0..=20).step_by(2).map(block).collect(), 0)
    }

    #[tokio::test]
    async fn position_at_finds_the_last_block_at_or_before() {
        let chain = even();
        let at = |time| {
            let chain = &chain;
            async move { chain.position_at(time).await.unwrap().map(|h| h.position) }
        };
        assert_eq!(at(99).await, None);
        assert_eq!(at(100).await, Some(0));
        assert_eq!(at(107).await, Some(6));
        assert_eq!(at(108).await, Some(8));
        assert_eq!(at(119).await, Some(18));
        assert_eq!(at(500).await, Some(20));
        assert_eq!(chain.tip().await.unwrap().position, 20);
    }

    #[tokio::test]
    async fn search_starts_at_the_first_served_block() {
        let chain = even();
        chain.prune(7);
        let at = |time| {
            let chain = &chain;
            async move { chain.position_at(time).await.unwrap().map(|h| h.position) }
        };
        // Pruned below 7: the first block it serves is 8.
        assert_eq!(at(105).await, None);
        assert_eq!(at(108).await, Some(8));
        assert_eq!(at(113).await, Some(12));
    }

    #[tokio::test]
    async fn search_tells_each_narrowing_range() {
        let chain = even();
        let probes = std::sync::Mutex::new(Vec::new());
        let found = chain
            .search(107, &|probe| probes.lock().unwrap().push(probe))
            .await
            .unwrap();
        assert_eq!(found.map(|stamp| stamp.position), Some(6));
        let probes = probes.into_inner().unwrap();
        let ranges: Vec<_> = probes
            .iter()
            .map(|probe| (probe.low.position, probe.high.position))
            .collect();
        assert_eq!(ranges.first(), Some(&(0, 20)));
        assert_eq!(ranges.last(), Some(&(6, 8)));
        for pair in ranges.windows(2) {
            assert!(
                pair[1].0 >= pair[0].0 && pair[1].1 <= pair[0].1,
                "{ranges:?}"
            );
        }
        for probe in &probes {
            assert!(probe.low.timestamp <= 107 && probe.high.timestamp > 107);
        }
    }
}
