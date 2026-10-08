//! Blocks as retained evidence ([`RawBlock`]) and as parsed facts ([`Block`]).
//!
//! `RawBlock` → [`RawBlock::parse`] → [`Block`] → [`Transaction`] →
//! [`Transaction::movements`] → [`Movement`]. Parsing happens only here.

mod output;
mod transaction;

use std::collections::{BTreeMap, BTreeSet};

use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use serde_json::Value;

pub use output::{COINBASE_MATURITY, Outpoint, Output, Utxo};
pub use transaction::{Fee, Input, Movement, Transaction};

use crate::native::{Error, Network, json::Object, rpc::wire::parse_block_hash};

/// One parsed block: its reference and transactions in block order.
pub type Block = base::Block<Transaction>;
/// Transactions of one block in block order, with unique ids.
pub type Transactions = base::Transactions<Transaction>;

/// Exact source evidence for one block, retained for replay and reorg recovery.
///
/// The payload is Bitcoin Core's `getblock` verbosity-2 JSON with every
/// input's previous output resolved into a compact `prevout` (value and
/// address), plus the network that gives addresses their text. Keeping bytes
/// rather than parsed facts makes replay independent of this parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    reference: BlockRef,
    network: Network,
    block: Vec<u8>,
}

impl RawBlock {
    /// Pairs evidence with the reference it claims; [`Self::parse`] checks both agree.
    #[must_use]
    pub const fn new(reference: BlockRef, network: Network, block: Vec<u8>) -> Self {
        Self {
            reference,
            network,
            block,
        }
    }

    /// The block this evidence claims to be.
    #[must_use]
    pub const fn reference(&self) -> &BlockRef {
        &self.reference
    }

    /// The network whose address encoding the evidence uses.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// The enriched `getblock` JSON bytes.
    #[must_use]
    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// Validates the evidence and decodes it into a [`Block`].
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation.
    pub fn parse(&self) -> Result<Block, Error> {
        let value: Value = serde_json::from_slice(&self.block)
            .map_err(|_| Error::invalid_block("Bitcoin block result is not valid JSON"))?;
        let object = Object::new(&value, "Bitcoin block result", Error::invalid_block)?;
        let reference = Self::claimed_reference(&object, None, None)?;
        if reference != self.reference {
            return Err(Error::invalid_block(
                "retained Bitcoin block reference does not match its raw payload",
            ));
        }
        let transactions = Self::transactions(&object, reference.height.0, self.network)?;
        let transactions = Transactions::new(transactions).map_err(|_| {
            Error::invalid_block("Bitcoin block contains duplicate transaction IDs")
        })?;
        Ok(Block::new(&self.network, reference, transactions))
    }

    fn transactions(
        object: &Object<'_>,
        height: u64,
        network: Network,
    ) -> Result<Vec<Transaction>, Error> {
        let values = object.array("tx", "Bitcoin block transactions")?;
        let declared = object.u64("nTx", "Bitcoin block transaction count")?;
        if usize::try_from(declared).ok() != Some(values.len()) {
            return Err(
                object.fail("Bitcoin block transaction count does not match its transaction array")
            );
        }
        let mut earlier = BTreeMap::new();
        let mut ids = BTreeSet::new();
        let mut spent = BTreeSet::new();
        let mut transactions = Vec::with_capacity(values.len());
        for value in values {
            let transaction = Transaction::parse(value, height, network, &earlier)?;
            if !ids.insert(*transaction.id()) {
                return Err(object.fail("Bitcoin block contains duplicate transaction IDs"));
            }
            let mut spends = transaction
                .inputs
                .iter()
                .filter(|input| input.previous.is_some());
            if !spends.all(|input| spent.insert(input.outpoint)) {
                return Err(object.fail("Bitcoin block spends the same outpoint more than once"));
            }
            earlier.extend(transaction.created());
            transactions.push(transaction);
        }
        Ok(transactions)
    }
}

impl Transaction {
    /// This transaction's outputs keyed by the outpoints later spenders name.
    fn created(&self) -> impl Iterator<Item = (Outpoint, Output)> + '_ {
        (0_u32..).zip(&self.outputs).map(|(output_index, output)| {
            let outpoint = Outpoint {
                transaction_id: *self.id(),
                output_index,
            };
            (outpoint, output.clone())
        })
    }
}

impl RawBlock {
    /// The reference a `getblock` result claims, checked against the
    /// requested height and hash when given.
    pub(crate) fn claimed_reference(
        object: &Object<'_>,
        height: Option<BlockHeight>,
        hash: Option<&BlockHash>,
    ) -> Result<BlockRef, Error> {
        let actual = BlockHeight(object.u64("height", "Bitcoin block height")?);
        if height.is_some_and(|expected| expected != actual) {
            return Err(object.fail("Bitcoin block height does not match the requested height"));
        }
        let actual_hash = Self::hash(object, object.string("hash", "Bitcoin block hash")?)?;
        if hash.is_some_and(|expected| expected != &actual_hash) {
            return Err(object.fail("Bitcoin block hash does not match the requested hash"));
        }
        let parent = Self::parent(object, actual)?;
        Ok(BlockRef {
            position: BlockPosition(actual.0),
            height: actual,
            hash: actual_hash,
            parent,
            timestamp: Some(object.u64("time", "Bitcoin block timestamp")?),
        })
    }

    fn parent(object: &Object<'_>, height: BlockHeight) -> Result<Option<BlockParent>, Error> {
        let claimed = object
            .get("previousblockhash")
            .is_some_and(|value| !value.is_null());
        let Some(position) = height.0.checked_sub(1) else {
            if claimed {
                return Err(object.fail("Bitcoin genesis block unexpectedly has a parent hash"));
            }
            return Ok(None);
        };
        let hash = object.string("previousblockhash", "Bitcoin previous block hash")?;
        Ok(Some(BlockParent {
            position: BlockPosition(position),
            hash: Self::hash(object, hash)?,
        }))
    }

    fn hash(object: &Object<'_>, text: &str) -> Result<BlockHash, Error> {
        parse_block_hash(text).ok_or_else(|| object.fail("Bitcoin block hash is invalid"))
    }
}
