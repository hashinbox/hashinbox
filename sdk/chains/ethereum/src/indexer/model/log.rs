use std::collections::BTreeSet;

use alloy_primitives::B256;
use indexing::BlockHeight;
use serde_json::{Map, Value};

use super::{Logs, ParseError, ParsedLog, required_address, required_hash, required_quantity_u64};

impl ParsedLog {
    pub(super) fn parse(
        value: &Value,
        block_height: BlockHeight,
        block_hash: [u8; 32],
        transaction_hash: [u8; 32],
        transaction_index: u64,
    ) -> Result<Self, ParseError> {
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Ethereum receipt log must be an object"))?;
        if required_hash(object, "blockHash", "log block hash")? != block_hash
            || required_quantity_u64(object, "blockNumber", "log block number")? != block_height.0
            || required_hash(object, "transactionHash", "log transaction hash")? != transaction_hash
            || required_quantity_u64(object, "transactionIndex", "log transaction index")?
                != transaction_index
        {
            return Err(ParseError::new(
                "Ethereum log identity does not match its receipt and block",
            ));
        }
        if object.get("removed").and_then(Value::as_bool) != Some(false) {
            return Err(ParseError::new(
                "canonical Ethereum receipt contains a removed log",
            ));
        }
        Self::parse_payload(object)
    }

    fn parse_payload(object: &Map<String, Value>) -> Result<Self, ParseError> {
        let address = required_address(object, "address", "log address")?;
        let topics = object
            .get("topics")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Ethereum log topics must be an array"))?
            .iter()
            .map(|topic| {
                let topic = topic
                    .as_str()
                    .ok_or_else(|| ParseError::new("Ethereum log topic must be a hash"))?;
                topic
                    .parse::<B256>()
                    .map(Into::into)
                    .map_err(|_| ParseError::new("Ethereum log topic is not a 32-byte hash"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let data = object
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::new("Ethereum log data must be a hex byte string"))?;
        let digits = data
            .strip_prefix("0x")
            .ok_or_else(|| ParseError::new("Ethereum log data is not hex encoded"))?;
        if digits.len() % 2 != 0 {
            return Err(ParseError::new(
                "Ethereum log data has an odd number of hex digits",
            ));
        }
        let data = digits
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair)
                    .map_err(|_| ParseError::new("Ethereum log data is not valid hex"))?;
                u8::from_str_radix(pair, 16)
                    .map_err(|_| ParseError::new("Ethereum log data is not valid hex"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let log_index = required_quantity_u64(object, "logIndex", "log index")?;

        Ok(Self {
            address,
            topics,
            data,
            log_index,
        })
    }
}

impl Logs {
    pub(super) fn parse(
        object: &Map<String, Value>,
        block_height: BlockHeight,
        block_hash: [u8; 32],
        transaction_hash: [u8; 32],
        transaction_index: u64,
        seen_log_indexes: &mut BTreeSet<u64>,
        previous_log_index: &mut Option<u64>,
    ) -> Result<Self, ParseError> {
        let log_values = object
            .get("logs")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Ethereum receipt logs must be an array"))?;
        let mut logs = Vec::with_capacity(log_values.len());
        for log_value in log_values {
            let log = ParsedLog::parse(
                log_value,
                block_height,
                block_hash,
                transaction_hash,
                transaction_index,
            )?;
            if !seen_log_indexes.insert(log.log_index) {
                return Err(ParseError::new(
                    "Ethereum block contains duplicate log indexes",
                ));
            }
            let out_of_order = previous_log_index.is_some_and(|previous| log.log_index <= previous);
            if out_of_order {
                return Err(ParseError::new(
                    "Ethereum logs are not ordered by log index",
                ));
            }
            *previous_log_index = Some(log.log_index);
            logs.push(log);
        }

        Ok(Self(logs))
    }
}
