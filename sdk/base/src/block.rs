use crate::Network;

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

/// One block of one network: its identity and its transactions in block order.
///
/// The network is stated by whoever builds the block and is never derived
/// from the header or the transactions: a [`Header`] is the same bytes
/// on every network that shares a history, so nothing inside it can say which
/// one was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block<T> {
    network: Network,
    header: Header,
    transactions: Vec<T>,
}

impl<T> Block<T> {
    /// Places a block identity and its transactions in `network`.
    #[must_use]
    pub fn new(network: impl Into<Network>, header: Header, transactions: Vec<T>) -> Self {
        Self {
            network: network.into(),
            header,
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
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// The block's transactions in block order.
    #[must_use]
    pub fn transactions(&self) -> &[T] {
        &self.transactions
    }

    /// Takes the block apart, for a stage that rebuilds it over another
    /// transaction type.
    #[must_use]
    pub fn into_parts(self) -> (Network, Header, Vec<T>) {
        (self.network, self.header, self.transactions)
    }
}
