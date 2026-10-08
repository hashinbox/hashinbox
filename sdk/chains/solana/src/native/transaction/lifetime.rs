use solana_hash::Hash;

/// A recent blockhash and the last block height at which a transaction that
/// references it may still be included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lifetime {
    blockhash: Hash,
    last_valid_block_height: u64,
}

impl Lifetime {
    /// `blockhash`, valid through block height `last_valid_block_height`.
    #[must_use]
    pub fn new(blockhash: Hash, last_valid_block_height: u64) -> Self {
        Self {
            blockhash,
            last_valid_block_height,
        }
    }

    /// The recent blockhash a message references.
    #[must_use]
    pub fn blockhash(&self) -> &Hash {
        &self.blockhash
    }

    /// The last block height that may include the transaction.
    #[must_use]
    pub fn last_valid_block_height(&self) -> u64 {
        self.last_valid_block_height
    }

    /// Whether a chain at block `height` can no longer include the transaction.
    #[must_use]
    pub fn expired(&self, height: u64) -> bool {
        height > self.last_valid_block_height
    }
}
