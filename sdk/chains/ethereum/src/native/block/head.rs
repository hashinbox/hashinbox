//! First parse stage: block header and transaction objects, before receipts.

use std::collections::BTreeSet;

use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use serde_json::Value;

use super::field::{
    Object, optional_address, required_address, required_hash, required_quantity,
    required_quantity_u64,
};
use crate::native::{Address, Error, TransactionId, Wei};

/// A block's identity and its transaction objects, without receipts.
///
/// The RPC layer reads it to know which receipts to fetch; `RawBlock::parse`
/// completes it with the receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) reference: BlockRef,
    pub(super) transactions: Vec<ListedTransaction>,
}

/// One transaction as its block lists it, before its receipt: the fields of
/// the transaction object that indexing consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedTransaction {
    pub(super) id: TransactionId,
    /// The sender, who pays the fee.
    pub from: Address,
    /// `None` for contract creation.
    pub to: Option<Address>,
    /// Ether attached to the transaction.
    pub value: Wei,
    /// Position in the block.
    pub index: u64,
}

impl Head {
    /// Parses one `eth_getBlockByNumber` result.
    ///
    /// With `full`, every transaction must be a complete object; otherwise
    /// hash-only listings are accepted and skipped.
    pub(crate) fn parse(
        raw: &[u8],
        expected_height: Option<BlockHeight>,
        full: bool,
    ) -> Result<Self, Error> {
        // Alloy owns the header shape, fork fields, and quantity decoding.
        // Transaction objects stay raw values so only consumed fields are checked.
        let block: RpcBlock<Value> = serde_json::from_slice(raw).map_err(|_| {
            Error::invalid_block("Ethereum block result does not match the RPC block shape")
        })?;
        let height = BlockHeight(block.number());
        if expected_height.is_some_and(|expected| expected != height) {
            return Err(Error::invalid_block(
                "Ethereum block number does not match the requested height",
            ));
        }
        let hash = block.hash().0;
        let reference = BlockRef {
            position: BlockPosition(height.0),
            height,
            hash: BlockHash(hash.to_vec()),
            parent: (height.0 != 0).then(|| BlockParent {
                position: BlockPosition(height.0 - 1),
                hash: BlockHash(block.header.parent_hash.0.to_vec()),
            }),
            timestamp: Some(block.header.timestamp),
        };
        let transactions = ListedTransaction::parse_all(block.transactions, height, hash, full)?;
        Ok(Self {
            reference,
            transactions,
        })
    }

    /// Transaction hashes in block order, for receipt lookups.
    pub(crate) fn ids(&self) -> impl Iterator<Item = &TransactionId> {
        self.transactions.iter().map(|listed| &listed.id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }
}

impl ListedTransaction {
    fn parse_all(
        transactions: BlockTransactions<Value>,
        height: BlockHeight,
        hash: [u8; 32],
        full: bool,
    ) -> Result<Vec<Self>, Error> {
        let values = match transactions {
            BlockTransactions::Full(values) => values,
            BlockTransactions::Hashes(_) | BlockTransactions::Uncle if full => {
                return Err(Error::invalid_block(
                    "Ethereum full-block request returned transaction hashes only",
                ));
            }
            BlockTransactions::Hashes(_) | BlockTransactions::Uncle => Vec::new(),
        };
        let mut transactions = Vec::with_capacity(values.len());
        let mut ids = BTreeSet::new();
        for (position, value) in values.iter().enumerate() {
            if !full && !value.is_object() {
                continue;
            }
            let transaction = Self::parse(value, position, height, hash)?;
            if !ids.insert(transaction.id.clone()) {
                return Err(Error::invalid_block(
                    "Ethereum block contains duplicate transaction hashes",
                ));
            }
            transactions.push(transaction);
        }
        Ok(transactions)
    }

    fn parse(
        value: &Value,
        position: usize,
        block_height: BlockHeight,
        block_hash: [u8; 32],
    ) -> Result<Self, Error> {
        let object: &Object = value
            .as_object()
            .ok_or_else(|| Error::invalid_block("Ethereum transaction must be an object"))?;
        let id = TransactionId::from_bytes(required_hash(object, "hash", "transaction hash")?);
        let from = required_address(object, "from", "transaction sender")?;
        let to = optional_address(object, "to", "transaction recipient")?;
        let value = Wei::from_u256(required_quantity(object, "value", "transaction value")?);
        let index = required_quantity_u64(object, "transactionIndex", "transaction index")?;
        let expected_index = u64::try_from(position)
            .map_err(|_| Error::invalid_block("Ethereum transaction position exceeds u64"))?;
        if index != expected_index {
            return Err(Error::invalid_block(
                "Ethereum transaction index does not match block order",
            ));
        }
        if required_hash(object, "blockHash", "transaction block hash")? != block_hash {
            return Err(Error::invalid_block(
                "Ethereum transaction block hash does not match its block",
            ));
        }
        if required_quantity_u64(object, "blockNumber", "transaction block number")?
            != block_height.0
        {
            return Err(Error::invalid_block(
                "Ethereum transaction block number does not match its block",
            ));
        }
        Ok(Self {
            id,
            from,
            to,
            value,
            index,
        })
    }
}
