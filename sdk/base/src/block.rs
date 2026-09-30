use std::{collections::BTreeMap, fmt, ops, slice, vec};

use crate::{Network, Networked};

/// Number of produced blocks through one canonical block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHeight(pub u64);

impl BlockHeight {
    /// Returns the next produced height, or `None` at the numeric boundary.
    #[must_use]
    pub const fn checked_successor(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl From<u64> for BlockHeight {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<BlockHeight> for u64 {
    fn from(value: BlockHeight) -> Self {
        value.0
    }
}

/// Native monotonic coordinate used by a concrete chain's RPC interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockPosition(pub u64);

impl BlockPosition {
    /// Returns the next native coordinate, or `None` at the numeric boundary.
    #[must_use]
    pub const fn checked_successor(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl From<u64> for BlockPosition {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<BlockPosition> for u64 {
    fn from(value: BlockPosition) -> Self {
        value.0
    }
}

/// Canonical block identity in the chain's native byte order.
///
/// The base layer deliberately does not impose a fixed length: common chains
/// use 32 bytes, while future protocols may use a different digest format.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHash(pub Vec<u8>);

/// Atomic identity of one canonical block's parent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockParent {
    /// The parent's native coordinate.
    pub position: BlockPosition,
    /// The parent's hash, compared on the next block to detect a reorg.
    pub hash: BlockHash,
}

impl From<(BlockPosition, BlockHash)> for BlockParent {
    fn from((position, hash): (BlockPosition, BlockHash)) -> Self {
        Self { position, hash }
    }
}

impl From<BlockParent> for (BlockPosition, BlockHash) {
    fn from(parent: BlockParent) -> Self {
        (parent.position, parent.hash)
    }
}

/// Chain-independent reference required for canonical traversal and reorg
/// detection. Protocol-specific block fields stay in concrete chain crates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRef {
    /// Native coordinate (Bitcoin/Ethereum height, Solana slot).
    pub position: BlockPosition,
    /// Produced-block count, used for confirmations.
    pub height: BlockHeight,
    /// The block's own hash.
    pub hash: BlockHash,
    /// The parent block, or `None` for genesis.
    pub parent: Option<BlockParent>,
    /// Unix seconds, when the chain reports one.
    pub timestamp: Option<u64>,
}

/// A value with a stable identity inside its collection, such as one
/// transaction inside one block.
pub trait Identified {
    /// Native identity; `Display` is its canonical text.
    type Id: Ord + Clone + fmt::Debug + fmt::Display;

    /// The value's identity.
    fn id(&self) -> &Self::Id;
}

/// Transactions of one block in block order, with unique ids.
pub struct Transactions<T: Identified> {
    values: Vec<T>,
    index: BTreeMap<T::Id, usize>,
}

impl<T: Identified> Transactions<T> {
    /// Keeps `values` in the given order.
    ///
    /// # Errors
    ///
    /// Returns the first id that occurs more than once.
    pub fn new(values: Vec<T>) -> Result<Self, T::Id> {
        let mut index = BTreeMap::new();
        for (position, value) in values.iter().enumerate() {
            if index.insert(value.id().clone(), position).is_some() {
                return Err(value.id().clone());
            }
        }
        Ok(Self { values, index })
    }

    /// Finds a transaction by id.
    #[must_use]
    pub fn get(&self, id: &T::Id) -> Option<&T> {
        self.index.get(id).map(|position| &self.values[*position])
    }

    /// Iterates in block order.
    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.values.iter()
    }

    /// Number of transactions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the block has no transactions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The transaction at one place in block order, counted from zero over the
/// transactions this collection holds.
impl<T: Identified> ops::Index<usize> for Transactions<T> {
    type Output = T;

    fn index(&self, place: usize) -> &T {
        &self.values[place]
    }
}

impl<T: Identified> Default for Transactions<T> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            index: BTreeMap::new(),
        }
    }
}

impl<T: Identified + Clone> Clone for Transactions<T> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            index: self.index.clone(),
        }
    }
}

impl<T: Identified + fmt::Debug> fmt::Debug for Transactions<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(&self.values).finish()
    }
}

/// The index is derived from the values, so equal values mean equal collections.
impl<T: Identified + PartialEq> PartialEq for Transactions<T> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<T: Identified + Eq> Eq for Transactions<T> {}

impl<'a, T: Identified> IntoIterator for &'a Transactions<T> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

impl<T: Identified> IntoIterator for Transactions<T> {
    type Item = T;
    type IntoIter = vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.into_iter()
    }
}

/// One block of one network: its identity and its transactions in block order.
///
/// The network is stated by whoever builds the block and is never derived
/// from the reference or the transactions: a [`BlockRef`] is the same bytes
/// on every network that shares a history, so nothing inside it can say which
/// one was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block<T: Identified> {
    network: Network,
    reference: BlockRef,
    transactions: Transactions<T>,
}

impl<T: Identified> Block<T> {
    /// Places a block identity and its transactions in `network`.
    #[must_use]
    pub fn new(
        network: &impl Networked,
        reference: BlockRef,
        transactions: Transactions<T>,
    ) -> Self {
        Self {
            network: network.canonical(),
            reference,
            transactions,
        }
    }

    /// The network the block was read from.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// The block's identity and parent link.
    #[must_use]
    pub const fn reference(&self) -> &BlockRef {
        &self.reference
    }

    /// The block's transactions in block order.
    #[must_use]
    pub const fn transactions(&self) -> &Transactions<T> {
        &self.transactions
    }

    /// Takes the block apart, for a stage that rebuilds it over another
    /// transaction type.
    #[must_use]
    pub fn into_parts(self) -> (Network, BlockRef, Transactions<T>) {
        (self.network, self.reference, self.transactions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChainName;

    #[test]
    fn zero_coordinates_convert_and_advance() {
        let position = BlockPosition::from(0);
        let height = BlockHeight::from(0);

        assert_eq!(u64::from(position), 0);
        assert_eq!(u64::from(height), 0);
        assert_eq!(position.checked_successor(), Some(BlockPosition(1)));
        assert_eq!(height.checked_successor(), Some(BlockHeight(1)));
    }

    #[test]
    fn maximum_coordinates_have_no_successor() {
        assert_eq!(BlockPosition(u64::MAX).checked_successor(), None);
        assert_eq!(BlockHeight(u64::MAX).checked_successor(), None);
    }

    #[test]
    fn parent_conversion_keeps_position_and_hash_atomic() {
        let pair = (BlockPosition(42), BlockHash(vec![1, 2, 3]));
        let parent = BlockParent::from(pair.clone());

        assert_eq!(parent.position, pair.0);
        assert_eq!(parent.hash, pair.1);
        assert_eq!(<(BlockPosition, BlockHash)>::from(parent), pair);
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Fixture {
        id: u32,
        label: &'static str,
    }

    impl Identified for Fixture {
        type Id = u32;

        fn id(&self) -> &u32 {
            &self.id
        }
    }

    fn fixture(id: u32, label: &'static str) -> Fixture {
        Fixture { id, label }
    }

    #[test]
    fn transactions_keep_block_order_and_find_by_id() {
        let transactions = Transactions::new(vec![
            fixture(9, "first"),
            fixture(2, "second"),
            fixture(5, "third"),
        ])
        .expect("unique ids");

        let labels = transactions
            .iter()
            .map(|value| value.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["first", "second", "third"]);
        assert_eq!((&transactions).into_iter().count(), 3);
        assert_eq!(transactions.len(), 3);
        assert!(!transactions.is_empty());
        assert_eq!(
            transactions.get(&2).map(|value| value.label),
            Some("second")
        );
        assert_eq!(transactions.get(&7), None);
        assert_eq!(transactions[0].label, "first");
        assert_eq!(transactions[2].label, "third");
        let owned = transactions
            .into_iter()
            .map(|value| value.id)
            .collect::<Vec<_>>();
        assert_eq!(owned, [9, 2, 5]);
    }

    #[test]
    fn transactions_reject_the_first_duplicate_id() {
        let duplicate = Transactions::new(vec![
            fixture(1, "a"),
            fixture(2, "b"),
            fixture(1, "c"),
            fixture(2, "d"),
        ]);

        assert_eq!(duplicate, Err(1));
    }

    fn reference() -> BlockRef {
        BlockRef {
            position: BlockPosition(4),
            height: BlockHeight(3),
            hash: BlockHash(vec![4]),
            parent: None,
            timestamp: None,
        }
    }

    fn network(name: &str) -> Network {
        Network::new(ChainName::new("example"), name)
    }

    #[test]
    fn empty_block_exposes_reference_and_no_transactions() {
        let block = Block::<Fixture>::new(&network("main"), reference(), Transactions::default());

        assert_eq!(block.reference(), &reference());
        assert!(block.transactions().is_empty());
        assert_eq!(block.clone(), block);
    }

    /// The network is part of what a block is: the same reference and the
    /// same transactions read from two networks are two blocks.
    #[test]
    fn a_block_carries_the_network_it_was_built_with() {
        let transactions =
            || Transactions::new(vec![fixture(1, "a"), fixture(2, "b")]).expect("unique ids");
        let main = Block::new(&network("main"), reference(), transactions());
        let test = Block::new(&network("test"), reference(), transactions());

        assert_eq!(main.network(), &network("main"));
        assert_eq!(test.network(), &network("test"));
        assert_ne!(main, test);

        let (owner, identity, children) = main.into_parts();
        assert_eq!(owner, network("main"));
        assert_eq!(identity, reference());
        assert_eq!(children, transactions());
    }
}
