use std::collections::BTreeSet;

use alloy_primitives::U256;
use indexing::BlockHeight;
use serde_json::{Map, Value};

use super::{
    Logs, ParseError, ParsedBlock, ParsedReceipt, ParsedTransaction, optional_address,
    required_address, required_hash, required_quantity, required_quantity_u64,
};

impl ParsedReceipt {
    pub(in crate::indexer) fn parse_all(
        raw_receipts: &[Vec<u8>],
        block: &ParsedBlock,
    ) -> Result<Vec<ParsedReceipt>, ParseError> {
        if raw_receipts.len() != block.transactions.len() {
            return Err(ParseError::new(
                "Ethereum receipt count does not match transaction count",
            ));
        }

        let block_hash: [u8; 32] = block
            .reference
            .hash
            .0
            .as_slice()
            .try_into()
            .map_err(|_| ParseError::new("Ethereum block hash is not 32 bytes"))?;
        let mut receipts = Vec::with_capacity(raw_receipts.len());
        let mut seen_log_indexes = BTreeSet::new();
        let mut previous_log_index = None;
        for (position, (raw, transaction)) in
            raw_receipts.iter().zip(&block.transactions).enumerate()
        {
            receipts.push(Self::parse(
                raw,
                transaction,
                position,
                block.reference.height,
                block_hash,
                &mut seen_log_indexes,
                &mut previous_log_index,
            )?);
        }
        Ok(receipts)
    }

    pub(super) fn parse(
        raw: &[u8],
        transaction: &ParsedTransaction,
        position: usize,
        block_height: BlockHeight,
        block_hash: [u8; 32],
        seen_log_indexes: &mut BTreeSet<u64>,
        previous_log_index: &mut Option<u64>,
    ) -> Result<Self, ParseError> {
        let value: Value = serde_json::from_slice(raw)
            .map_err(|_| ParseError::new("Ethereum receipt result is not valid JSON"))?;
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Ethereum receipt must be an object"))?;
        transaction.validate_receipt(object, position, block_height, block_hash)?;
        Self::parse_execution(
            object,
            transaction,
            block_height,
            block_hash,
            seen_log_indexes,
            previous_log_index,
        )
    }

    fn parse_execution(
        object: &Map<String, Value>,
        transaction: &ParsedTransaction,
        block_height: BlockHeight,
        block_hash: [u8; 32],
        seen_log_indexes: &mut BTreeSet<u64>,
        previous_log_index: &mut Option<u64>,
    ) -> Result<Self, ParseError> {
        let status = required_quantity(object, "status", "receipt status")?;
        let succeeded = if status == U256::ZERO {
            false
        } else if status == U256::from(1_u8) {
            true
        } else {
            return Err(ParseError::new(
                "Ethereum receipt status must be zero or one",
            ));
        };
        let gas_used = required_quantity_u64(object, "gasUsed", "receipt gas used")?;
        let effective_gas_price =
            required_quantity(object, "effectiveGasPrice", "receipt effective gas price")?;
        let contract_address =
            optional_address(object, "contractAddress", "receipt contract address")?;
        transaction.validate_creation(succeeded, contract_address)?;

        let logs = Logs::parse(
            object,
            block_height,
            block_hash,
            transaction.hash,
            transaction.index,
            seen_log_indexes,
            previous_log_index,
        )?
        .0;

        Ok(Self {
            transaction_hash: transaction.hash,
            transaction_index: transaction.index,
            succeeded,
            gas_used,
            effective_gas_price,
            contract_address,
            logs,
        })
    }
}

impl ParsedTransaction {
    fn validate_receipt(
        &self,
        object: &Map<String, Value>,
        position: usize,
        block_height: BlockHeight,
        block_hash: [u8; 32],
    ) -> Result<(), ParseError> {
        let transaction_hash = required_hash(object, "transactionHash", "receipt transaction")?;
        if transaction_hash != self.hash {
            return Err(ParseError::new(
                "Ethereum receipt transaction hash does not match block order",
            ));
        }
        let transaction_index =
            required_quantity_u64(object, "transactionIndex", "receipt transaction index")?;
        if transaction_index != self.index
            || transaction_index
                != u64::try_from(position)
                    .map_err(|_| ParseError::new("Ethereum receipt position exceeds u64"))?
        {
            return Err(ParseError::new(
                "Ethereum receipt transaction index does not match block order",
            ));
        }
        if required_hash(object, "blockHash", "receipt block hash")? != block_hash {
            return Err(ParseError::new(
                "Ethereum receipt block hash does not match its block",
            ));
        }
        if required_quantity_u64(object, "blockNumber", "receipt block number")? != block_height.0 {
            return Err(ParseError::new(
                "Ethereum receipt block number does not match its block",
            ));
        }
        if required_address(object, "from", "receipt sender")? != self.from {
            return Err(ParseError::new(
                "Ethereum receipt sender does not match its transaction",
            ));
        }
        if optional_address(object, "to", "receipt recipient")? != self.to {
            return Err(ParseError::new(
                "Ethereum receipt recipient does not match its transaction",
            ));
        }

        Ok(())
    }

    fn validate_creation(
        &self,
        succeeded: bool,
        contract_address: Option<[u8; 20]>,
    ) -> Result<(), ParseError> {
        if self.to.is_some() && contract_address.is_some() {
            return Err(ParseError::new(
                "non-creation receipt unexpectedly contains a contract address",
            ));
        }
        if succeeded && self.to.is_none() && !self.value.is_zero() && contract_address.is_none() {
            return Err(ParseError::new(
                "successful value-bearing contract creation has no contract address",
            ));
        }

        Ok(())
    }
}
