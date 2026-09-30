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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::native::ErrorKind;

    fn raw(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    fn block() -> Value {
        json!({
            "blockhash": "11111111111111111111111111111111",
            "previousBlockhash": "11111111111111111111111111111111",
            "parentSlot": 6,
            "transactions": [],
            "blockTime": 123,
            "blockHeight": 4,
        })
    }

    fn cluster(slug: &str) -> Network {
        let genesis = "11111111111111111111111111111111";
        Network::new(slug, genesis.parse().unwrap())
    }

    fn retained(position: u64, bytes: Vec<u8>) -> Result<RawBlock, Error> {
        RawBlock::new(BlockPosition(position), cluster("localnet"), bytes)
    }

    fn message(position: u64, value: Value) -> String {
        retained(position, raw(value)).unwrap_err().message
    }

    /// The parsed block belongs to the network its evidence was read from,
    /// and to no other: the same bytes retained for two clusters parse into
    /// two different blocks.
    #[test]
    fn a_parsed_block_carries_the_network_of_its_evidence() {
        let localnet = retained(7, raw(block())).unwrap().parse().unwrap();
        assert_eq!(localnet.network().chain, crate::native::CHAIN_NAME);
        assert_eq!(localnet.network().name, "localnet");

        let elsewhere = RawBlock::new(BlockPosition(7), cluster("devnet"), raw(block()));
        let devnet = elsewhere.unwrap().parse().unwrap();
        assert_eq!(devnet.network().name, "devnet");
        assert_ne!(localnet, devnet);
    }

    #[test]
    fn hash_validation_keeps_bytes_field_context_and_precedes_height() {
        let mut value = block();
        let bytes = [255; 32];
        value["blockhash"] = json!(Hash::new_from_array(bytes).to_string());
        let parsed = retained(7, raw(value.clone())).unwrap();
        assert_eq!(parsed.reference().hash.0, bytes);
        value["blockHeight"] = json!(null);
        for invalid in [
            "",
            "0",
            "1111111111111111111111111111111",
            "111111111111111111111111111111111",
            "é",
        ] {
            value["blockhash"] = json!(invalid);
            value["previousBlockhash"] = json!(invalid);
            assert_eq!(
                message(7, value.clone()),
                "Solana blockhash is not a canonical hash"
            );
            value["blockhash"] = json!(Hash::new_from_array(bytes).to_string());
            assert_eq!(
                message(7, value.clone()),
                "Solana previous blockhash is not a canonical hash"
            );
        }
    }

    #[test]
    fn retains_exact_raw_result_and_builds_sparse_reference() {
        let bytes = raw(block());
        let parsed = retained(7, bytes.clone()).unwrap();
        assert_eq!(parsed.bytes(), bytes);
        assert_eq!(parsed.reference().position, BlockPosition(7));
        assert_eq!(parsed.reference().height, BlockHeight(4));
        assert_eq!(parsed.reference().timestamp, Some(123));
        assert_eq!(
            parsed.reference().parent.as_ref().unwrap().position,
            BlockPosition(6)
        );
        let block = parsed.parse().unwrap();
        assert_eq!(block.reference(), parsed.reference());
        assert_eq!(block.transactions().len(), 0);
    }

    #[test]
    fn requires_height_transactions_canonical_hashes_and_strict_parent() {
        let mut cases = Vec::new();
        for (field, value) in [
            ("blockhash", json!("bad")),
            ("previousBlockhash", json!("bad")),
            ("parentSlot", json!(7)),
            ("blockHeight", Value::Null),
        ] {
            let mut malformed = block();
            malformed[field] = value;
            cases.push(malformed);
        }
        let mut missing = block();
        missing.as_object_mut().unwrap().remove("transactions");
        cases.push(missing);
        for malformed in cases {
            let error = retained(7, raw(malformed)).unwrap_err();
            assert_eq!(
                (error.kind, error.retryable),
                (ErrorKind::InvalidBlock, false)
            );
        }
    }

    #[test]
    fn accepts_only_a_zero_parent_for_genesis_and_non_negative_time() {
        let mut genesis = block();
        genesis["parentSlot"] = json!(0);
        assert!(retained(0, raw(genesis)).is_ok());

        let mut bad_genesis = block();
        bad_genesis["parentSlot"] = json!(1);
        assert_eq!(
            message(0, bad_genesis),
            "Solana genesis parent slot must be zero"
        );

        let mut negative_time = block();
        negative_time["blockTime"] = json!(-1);
        assert_eq!(
            message(7, negative_time),
            "Solana block time must not be negative"
        );
    }
}
