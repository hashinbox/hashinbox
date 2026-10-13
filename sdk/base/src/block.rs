use std::collections::BTreeSet;

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
}

/// Where indexed blocks are kept.
pub trait Store: Send + Sync {
    /// Keeps `blocks` (one network, ascending, as the chain parsed them), in
    /// one atomic write. Against its own indexes, read in that write, the
    /// store keeps only the legs and rewards of the indexed addresses each
    /// block applies to (birthday at or below it, indexed up to its parent),
    /// and moves those indexes to it, empty blocks included. A repeat is a
    /// no-op.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn insert<'a>(&'a self, blocks: &'a [Block]) -> Future<'a, anyhow::Result<()>>;

    /// The last kept block of `network`; `None` before the first.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn get_last<'a>(&'a self, network: &'a Network) -> Future<'a, anyhow::Result<Option<Header>>>;

    /// Forgets everything kept for `network`.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn delete<'a>(&'a self, network: &'a Network) -> Future<'a, anyhow::Result<()>>;
}

/// Tells listeners about each block a [`Store`] kept.
pub trait Notify: Send + Sync {
    /// Calls `listener` after each kept block.
    fn subscribe(&self, listener: Box<dyn Fn(&Header) + Send + Sync>);
}
