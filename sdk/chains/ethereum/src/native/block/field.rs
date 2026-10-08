//! Typed reads of JSON-RPC object fields with contextual error messages.

use alloy_primitives::{B256, U256};
use serde_json::{Map, Value};

use crate::native::{Address, Error};

pub(super) type Object = Map<String, Value>;

pub(super) fn required_hash(object: &Object, key: &str, label: &str) -> Result<[u8; 32], Error> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid_block(format!("Ethereum {label} is missing")))
        .and_then(|value| {
            value.parse::<B256>().map(Into::into).map_err(|_| {
                Error::invalid_block(format!("Ethereum {label} is not a 32-byte hash"))
            })
        })
}

pub(super) fn required_address(object: &Object, key: &str, label: &str) -> Result<Address, Error> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid_block(format!("Ethereum {label} is missing")))
        .and_then(|value| address(value, label))
}

pub(super) fn optional_address(
    object: &Object,
    key: &str,
    label: &str,
) -> Result<Option<Address>, Error> {
    match object.get(key) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(value)) => address(value, label).map(Some),
        Some(_) => Err(Error::invalid_block(format!(
            "Ethereum {label} must be an address or null"
        ))),
    }
}

pub(super) fn required_quantity(object: &Object, key: &str, label: &str) -> Result<U256, Error> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid_block(format!("Ethereum {label} is missing")))
        .and_then(|value| quantity(value, label))
}

pub(super) fn required_quantity_u64(object: &Object, key: &str, label: &str) -> Result<u64, Error> {
    let value = required_quantity(object, key, label)?;
    u64::try_from(value).map_err(|_| Error::invalid_block(format!("Ethereum {label} exceeds u64")))
}

/// Alloy's address grammar: optional `0x`/`0X`, exactly 20 bytes of hex.
fn address(value: &str, label: &str) -> Result<Address, Error> {
    value
        .parse::<alloy_primitives::Address>()
        .map(|address| Address::from_bytes(address.into_array()))
        .map_err(|_| Error::invalid_block(format!("Ethereum {label} is not a 20-byte address")))
}

/// Canonical `0x` quantity without leading zeroes, up to 256 bits.
fn quantity(value: &str, label: &str) -> Result<U256, Error> {
    let digits = value
        .strip_prefix("0x")
        .ok_or_else(|| Error::invalid_block(format!("Ethereum {label} is not a hex quantity")))?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return Err(Error::invalid_block(format!(
            "Ethereum {label} is not a canonical hex quantity"
        )));
    }
    U256::from_str_radix(digits, 16)
        .map_err(|_| Error::invalid_block(format!("Ethereum {label} exceeds 256 bits")))
}
