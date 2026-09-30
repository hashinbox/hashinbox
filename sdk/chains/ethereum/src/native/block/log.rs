use std::collections::BTreeSet;

use alloy_primitives::B256;
use serde_json::Value;

use super::{
    field::{Object, required_address, required_hash, required_quantity_u64},
    receipt::Position,
};
use crate::native::{Address, Error};

/// One event log emitted by a successful or failed transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    /// The emitting contract.
    pub address: Address,
    /// Indexed topics; topic 0 is the event signature.
    pub topics: Vec<[u8; 32]>,
    /// Unindexed ABI-encoded event data.
    pub data: Vec<u8>,
    /// Block-wide log index.
    pub index: u64,
}

/// Log indexes already seen in one block: they must be unique and increasing.
#[derive(Debug, Default)]
pub(super) struct Indexes {
    seen: BTreeSet<u64>,
    previous: Option<u64>,
}

impl Log {
    pub(super) fn parse_all(
        receipt: &Object,
        position: &Position<'_>,
        indexes: &mut Indexes,
    ) -> Result<Vec<Self>, Error> {
        receipt
            .get("logs")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::invalid_block("Ethereum receipt logs must be an array"))?
            .iter()
            .map(|value| {
                let log = Self::parse(value, position)?;
                indexes.admit(log.index)?;
                Ok(log)
            })
            .collect()
    }

    fn parse(value: &Value, position: &Position<'_>) -> Result<Self, Error> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::invalid_block("Ethereum receipt log must be an object"))?;
        let transaction_index = u64::try_from(position.index).ok();
        if required_hash(object, "blockHash", "log block hash")? != position.block_hash
            || required_quantity_u64(object, "blockNumber", "log block number")?
                != position.height.0
            || required_hash(object, "transactionHash", "log transaction hash")?
                != *position.listed.id.as_bytes()
            || Some(required_quantity_u64(
                object,
                "transactionIndex",
                "log transaction index",
            )?) != transaction_index
        {
            return Err(Error::invalid_block(
                "Ethereum log identity does not match its receipt and block",
            ));
        }
        if object.get("removed").and_then(Value::as_bool) != Some(false) {
            return Err(Error::invalid_block(
                "canonical Ethereum receipt contains a removed log",
            ));
        }
        Self::parse_payload(object)
    }

    fn parse_payload(object: &Object) -> Result<Self, Error> {
        let address = required_address(object, "address", "log address")?;
        let topics = object
            .get("topics")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::invalid_block("Ethereum log topics must be an array"))?
            .iter()
            .map(|topic| {
                topic
                    .as_str()
                    .ok_or_else(|| Error::invalid_block("Ethereum log topic must be a hash"))?
                    .parse::<B256>()
                    .map(Into::into)
                    .map_err(|_| Error::invalid_block("Ethereum log topic is not a 32-byte hash"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let data = Self::parse_data(object)?;
        let index = required_quantity_u64(object, "logIndex", "log index")?;
        Ok(Self {
            address,
            topics,
            data,
            index,
        })
    }

    fn parse_data(object: &Object) -> Result<Vec<u8>, Error> {
        let data = object
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid_block("Ethereum log data must be a hex byte string"))?;
        let digits = data
            .strip_prefix("0x")
            .ok_or_else(|| Error::invalid_block("Ethereum log data is not hex encoded"))?;
        if digits.len() % 2 != 0 {
            return Err(Error::invalid_block(
                "Ethereum log data has an odd number of hex digits",
            ));
        }
        digits
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                std::str::from_utf8(pair)
                    .ok()
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or_else(|| Error::invalid_block("Ethereum log data is not valid hex"))
            })
            .collect()
    }
}

impl Indexes {
    /// Accepts `index` only when it is new and above the previous one.
    fn admit(&mut self, index: u64) -> Result<(), Error> {
        if self.seen.contains(&index) {
            return Err(Error::invalid_block(
                "Ethereum block contains duplicate log indexes",
            ));
        }
        if self.previous.is_some_and(|previous| index <= previous) {
            return Err(Error::invalid_block(
                "Ethereum logs are not ordered by log index",
            ));
        }
        self.seen.insert(index);
        self.previous = Some(index);
        Ok(())
    }
}
