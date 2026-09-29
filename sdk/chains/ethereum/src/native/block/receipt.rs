use alloy_primitives::U256;
use base::BlockHeight;
use serde_json::Value;

use super::{
    Log,
    field::{
        Object, optional_address, required_address, required_hash, required_quantity,
        required_quantity_u64,
    },
    head::{Head, Sent},
    log::Order,
};
use crate::native::{Address, Error, Wei};

/// Execution result of one transaction, from `eth_getTransactionReceipt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// `status == 1`; a failed transaction still pays its fee.
    pub succeeded: bool,
    /// Gas actually consumed.
    pub gas_used: u64,
    /// Wei paid per unit of gas.
    pub effective_gas_price: Wei,
    /// The created contract, for a transaction without `to`.
    pub contract_address: Option<Address>,
    /// Logs in block-wide log-index order.
    pub logs: Vec<Log>,
}

/// Where one receipt must sit: its transaction and block.
pub(super) struct Position<'a> {
    pub(super) sent: &'a Sent,
    pub(super) index: usize,
    pub(super) height: BlockHeight,
    pub(super) block_hash: [u8; 32],
}

impl Receipt {
    /// Parses one receipt per transaction of `head`, in block order.
    pub(super) fn parse_all(receipts: &[Vec<u8>], head: &Head) -> Result<Vec<Self>, Error> {
        if receipts.len() != head.transactions.len() {
            return Err(Error::invalid_block(
                "Ethereum receipt count does not match transaction count",
            ));
        }
        let block_hash: [u8; 32] = head
            .reference
            .hash
            .0
            .as_slice()
            .try_into()
            .map_err(|_| Error::invalid_block("Ethereum block hash is not 32 bytes"))?;
        let mut order = Order::default();
        receipts
            .iter()
            .zip(&head.transactions)
            .enumerate()
            .map(|(index, (raw, sent))| {
                let position = Position {
                    sent,
                    index,
                    height: head.reference.height,
                    block_hash,
                };
                Self::parse(raw, &position, &mut order)
            })
            .collect()
    }

    pub(super) fn parse(
        raw: &[u8],
        position: &Position<'_>,
        order: &mut Order,
    ) -> Result<Self, Error> {
        let value: Value = serde_json::from_slice(raw)
            .map_err(|_| Error::invalid_block("Ethereum receipt result is not valid JSON"))?;
        let object = value
            .as_object()
            .ok_or_else(|| Error::invalid_block("Ethereum receipt must be an object"))?;
        position.validate(object)?;
        let status = required_quantity(object, "status", "receipt status")?;
        let succeeded = if status == U256::ZERO {
            false
        } else if status == U256::from(1_u8) {
            true
        } else {
            return Err(Error::invalid_block(
                "Ethereum receipt status must be zero or one",
            ));
        };
        let gas_used = required_quantity_u64(object, "gasUsed", "receipt gas used")?;
        let effective_gas_price =
            required_quantity(object, "effectiveGasPrice", "receipt effective gas price")?;
        let contract_address =
            optional_address(object, "contractAddress", "receipt contract address")?;
        position
            .sent
            .validate_creation(succeeded, contract_address.as_ref())?;
        let logs = Log::parse_all(object, position, order)?;
        Ok(Self {
            succeeded,
            gas_used,
            effective_gas_price: Wei::from_u256(effective_gas_price),
            contract_address,
            logs,
        })
    }
}

impl Position<'_> {
    fn validate(&self, object: &Object) -> Result<(), Error> {
        let sent = self.sent;
        if required_hash(object, "transactionHash", "receipt transaction")? != *sent.id.as_bytes() {
            return Err(Error::invalid_block(
                "Ethereum receipt transaction hash does not match block order",
            ));
        }
        let index = required_quantity_u64(object, "transactionIndex", "receipt transaction index")?;
        let expected = u64::try_from(self.index)
            .map_err(|_| Error::invalid_block("Ethereum receipt position exceeds u64"))?;
        if index != sent.index || index != expected {
            return Err(Error::invalid_block(
                "Ethereum receipt transaction index does not match block order",
            ));
        }
        if required_hash(object, "blockHash", "receipt block hash")? != self.block_hash {
            return Err(Error::invalid_block(
                "Ethereum receipt block hash does not match its block",
            ));
        }
        if required_quantity_u64(object, "blockNumber", "receipt block number")? != self.height.0 {
            return Err(Error::invalid_block(
                "Ethereum receipt block number does not match its block",
            ));
        }
        if required_address(object, "from", "receipt sender")? != sent.from {
            return Err(Error::invalid_block(
                "Ethereum receipt sender does not match its transaction",
            ));
        }
        if optional_address(object, "to", "receipt recipient")? != sent.to {
            return Err(Error::invalid_block(
                "Ethereum receipt recipient does not match its transaction",
            ));
        }
        Ok(())
    }
}

impl Sent {
    fn validate_creation(
        &self,
        succeeded: bool,
        contract_address: Option<&Address>,
    ) -> Result<(), Error> {
        if self.to.is_some() && contract_address.is_some() {
            return Err(Error::invalid_block(
                "non-creation receipt unexpectedly contains a contract address",
            ));
        }
        if succeeded && self.to.is_none() && !self.value.is_zero() && contract_address.is_none() {
            return Err(Error::invalid_block(
                "successful value-bearing contract creation has no contract address",
            ));
        }
        Ok(())
    }
}
