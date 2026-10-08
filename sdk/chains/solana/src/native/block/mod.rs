//! Blocks as retained evidence ([`RawBlock`]) and as parsed facts ([`Block`]).
//!
//! `RawBlock` → [`RawBlock::parse`] → [`Block`] → [`Transaction`] →
//! [`Transaction::movements`] → [`Movement`]. Parsing happens only here.

mod instruction;
mod movement;
mod token;
mod transaction;
mod wire;

use std::str::FromStr;

use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use solana_hash::Hash;

pub use instruction::Instruction;
pub use movement::Movement;
pub use token::TokenBalance;
pub use transaction::{AccountKey, Balance, Fee, Transaction};

use self::wire::Reply;
use crate::native::{Error, Network};

/// One parsed block: its reference and transactions in block order.
pub type Block = base::Block<Transaction>;
/// Transactions of one block in block order, with unique first signatures.
pub type Transactions = base::Transactions<Transaction>;

/// Exact source evidence for one finalized block, retained for replay and
/// reorg recovery.
///
/// The payload is the JSON-RPC `result` bytes of `getBlock` (`json`
/// encoding, full transactions, no rewards), plus the network they were read
/// from: nothing in the bytes names it, so it is stated by the client that
/// verified the genesis hash. Keeping bytes rather than parsed facts makes
/// replay independent of this parser; only the header is checked when the
/// block is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    reference: BlockRef,
    network: Network,
    bytes: Vec<u8>,
}

impl RawBlock {
    /// Checks the header of the `getBlock` result `bytes` for `position` and
    /// keeps the exact bytes, as read from `network`.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] for a non-canonical hash,
    /// a missing produced height, a negative time, or a parent slot that does
    /// not precede `position`.
    pub fn new(position: BlockPosition, network: Network, bytes: Vec<u8>) -> Result<Self, Error> {
        let wire = Reply::decode(&bytes)
            .map_err(|_| Error::invalid_block("Solana getBlock result has an invalid shape"))?;
        let reference = wire.reference(position)?;
        Ok(Self {
            reference,
            network,
            bytes,
        })
    }

    /// The block this evidence is.
    #[must_use]
    pub const fn reference(&self) -> &BlockRef {
        &self.reference
    }

    /// The network the evidence was read from.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// The `getBlock` result bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Validates every transaction and decodes the block.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation;
    /// one malformed transaction rejects the whole block.
    pub fn parse(&self) -> Result<Block, Error> {
        let wire = Reply::decode(&self.bytes).map_err(|_| {
            Error::invalid_block("Solana block transactions have an invalid RPC shape")
        })?;
        let mut transactions = wire
            .transactions()?
            .into_iter()
            .map(Transaction::parse)
            .collect::<Result<Vec<_>, _>>()?;
        for transaction in &mut transactions {
            transaction.decode_movements()?;
        }
        let transactions = Transactions::new(transactions).map_err(|_| {
            Error::invalid_block("Solana block contains duplicate first signatures")
        })?;
        Ok(Block::new(
            &self.network,
            self.reference.clone(),
            transactions,
        ))
    }

    /// Fails unless this block is the next produced block after `previous`:
    /// a later slot, the next height, and `previous` as its parent.
    pub(crate) fn follows(&self, previous: &Self) -> Result<(), Error> {
        let (previous, current) = (&previous.reference, &self.reference);
        let expected_height = previous
            .height
            .checked_successor()
            .ok_or_else(|| Error::rpc("Solana produced height is exhausted", true))?;
        let expected_parent = BlockParent {
            position: previous.position,
            hash: previous.hash.clone(),
        };
        if current.position <= previous.position
            || current.height != expected_height
            || current.parent.as_ref() != Some(&expected_parent)
        {
            return Err(Error::rpc(
                "Solana produced blocks are not a strict canonical sequence",
                true,
            ));
        }
        Ok(())
    }
}

impl Reply {
    fn reference(&self, position: BlockPosition) -> Result<BlockRef, Error> {
        let slot = position.0;
        let hash = Self::hash(&self.blockhash, "blockhash")?;
        let parent_hash = Self::hash(&self.previous_blockhash, "previous blockhash")?;
        let height = self
            .block_height
            .ok_or_else(|| Error::invalid_block("Solana finalized block has no produced height"))?;
        let timestamp = self
            .block_time
            .map(|value| {
                u64::try_from(value)
                    .map_err(|_| Error::invalid_block("Solana block time must not be negative"))
            })
            .transpose()?;
        let parent = match slot {
            0 if self.parent_slot != 0 => {
                return Err(Error::invalid_block(
                    "Solana genesis parent slot must be zero",
                ));
            }
            0 => None,
            _ if self.parent_slot >= slot => {
                return Err(Error::invalid_block(
                    "Solana block parent slot must precede its slot",
                ));
            }
            _ => Some(BlockParent {
                position: BlockPosition(self.parent_slot),
                hash: parent_hash,
            }),
        };
        Ok(BlockRef {
            position,
            height: BlockHeight(height),
            hash,
            parent,
            timestamp,
        })
    }

    /// A canonical Base58 hash; `field` names it in the error.
    fn hash(text: &str, field: &'static str) -> Result<BlockHash, Error> {
        let hash = Hash::from_str(text)
            .map_err(|_| Error::invalid_block(format!("Solana {field} is not a canonical hash")))?;
        if hash.to_string() != text {
            return Err(Error::invalid_block(format!(
                "Solana {field} is not canonically encoded"
            )));
        }
        Ok(BlockHash(hash.to_bytes().to_vec()))
    }
}
