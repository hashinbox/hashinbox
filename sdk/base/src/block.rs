use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::Context as _;
use tokio::sync::mpsc;

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

/// The newest produced and newest final positions of a chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tips {
    /// The newest produced block.
    pub produced: u64,
    /// The newest block the chain will not take back, by its own finality rule.
    pub confirmed: u64,
}

/// A chain that can be read block by block.
pub trait Chained: Networked + Send + Sync {
    /// The newest produced and newest final positions.
    ///
    /// # Errors
    ///
    /// When the node cannot answer.
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>>;

    /// The block at `position`, fully interpreted; `None` where no block was
    /// produced (a skipped Solana slot).
    ///
    /// # Errors
    ///
    /// When the node cannot answer or the block cannot be read.
    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>>;

    /// Blocks `start..=end`, linked, skipped positions omitted. Defaults to
    /// [`Chained::block`] one at a time; a chain may fetch them together.
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

/// Wakes the [`Indexer`](crate::Indexer) run: on a new block (a websocket
/// subscription), a timer, or anything else.
pub trait Notified: Send + Sync {
    /// A new channel of wakes; the run stops when it closes.
    fn subscribe(&self) -> mpsc::Receiver<()>;
}

/// Wakes every period; stops the moment its receiver is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer(pub std::time::Duration);

impl Notified for Timer {
    fn subscribe(&self) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel(1);
        let period = self.0;
        tokio::spawn(async move {
            // Ends as soon as the receiver is dropped, not a period later.
            while sender.send(()).await.is_ok() {
                if tokio::time::timeout(period, sender.closed()).await.is_ok() {
                    return;
                }
            }
        });
        receiver
    }
}

/// Where indexed blocks are kept: every block the [`Indexer`](crate::Indexer)
/// fetched, with the facts of indexed address assets, for every network.
///
/// # Model
///
/// - A block is *live* until [`Store::delete`] archives it. Archived blocks
///   stay (an `archived_at` mark), but their facts leave every read: balances,
///   history, unspent outputs.
/// - The live blocks of a network are one linked run: each names the one
///   below it as its parent. At most one live block per position.
/// - Each index, one asset of one address, carries `indexed_at`
///   ([`crate::WalletIndex`]): the address's facts in that asset are kept for
///   every live block from its birthday through that block, with no gaps.
///   Progress is per address and asset, so a new address or a newly
///   configured token backfills without disturbing the others.
/// - Blocks are kept up to the produced tip, not only the confirmed one. A
///   confirmed balance is a read over live blocks at or below
///   [`Tips::confirmed`]; a pending balance reads them all. The store records
///   no confirmation itself.
/// - The store never detects a reorg. The indexer compares the last live
///   block with the chain and calls [`Store::delete`]; the store only refuses
///   blocks that do not link onto what it holds.
pub trait Store: Send + Sync {
    /// Keeps `blocks` (one network, linked, as the chain parsed them, every
    /// address unfiltered), in one atomic write that:
    ///
    /// 1. Refuses the whole write when a block conflicts with what is kept: a
    ///    live block at its position with another hash, or a first new block
    ///    above the last live one whose parent is not that block.
    /// 2. Adds blocks it does not hold; blocks it holds are not added again.
    /// 3. For each block, reads its indexes inside this write and keeps the
    ///    legs, outputs and rewards whose (address, asset) is an index the
    ///    block applies to: birthday at or below it, and `indexed_at` at its
    ///    parent (or unset, when the block is the birthday or the first one at
    ///    or above it); a transaction is kept once, when any of its legs is.
    ///    Other facts are dropped.
    /// 4. Marks outputs spent by those legs as spent in that block.
    /// 5. Moves `indexed_at` of those indexes to the block, empty blocks
    ///    included, so progress stays contiguous.
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

    /// The last live block of `network`; `None` before the first, or when
    /// every block is archived.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_last<'a>(&'a self, network: &'a Network) -> Future<'a, anyhow::Result<Option<Header>>>;

    /// Archives every live block of `network` at or above `from`, in one
    /// atomic write that:
    ///
    /// 1. Sets `archived_at` on those blocks; nothing is hard-deleted. Their
    ///    transactions, legs, outputs and rewards leave every read.
    /// 2. Unmarks outputs spent in them: those outputs are unspent again.
    /// 3. Rewinds `indexed_at` of every index at or above `from` to the live
    ///    block below `from` (unset when none is), so those indexes re-index
    ///    the new branch.
    ///
    /// Other networks, wallets and indexes stay. Archiving nothing is a no-op.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn delete<'a>(&'a self, network: &'a Network, from: u64) -> Future<'a, anyhow::Result<()>>;
}

/// Tells listeners about each block a [`Store`] kept.
pub trait Notify: Send + Sync {
    /// Calls `listener` after each kept block.
    fn subscribe(&self, listener: Box<Listener>);
}

/// A [`Notify`] listener: called with each block's header after its write.
pub type Listener = dyn Fn(&Header) + Send + Sync;

/// A [`Store`] that tells its listeners about every block of each accepted
/// insert, after the write commits and outside every lock. A repeated insert
/// is heard again: listeners are hints, never transactional, and cannot veto.
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
            for block in blocks.iter() {
                listeners
                    .iter()
                    .for_each(|listener| listener(&block.header));
            }
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
