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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn quantity_preserves_library_grammar_and_field_errors() {
        for (text, expected) in [("0x0", 0), ("0xAbC", 0xabc), ("0x_", 0), ("0x1_0", 16)] {
            assert_eq!(quantity(text, "chain ID").unwrap(), U256::from(expected));
        }
        assert_eq!(
            quantity(&format!("0x{}", "f".repeat(64)), "value").unwrap(),
            U256::MAX
        );
        for (text, reason) in [
            ("0X1", "is not a hex quantity"),
            ("0x", "is not a canonical hex quantity"),
            ("0x0g", "is not a canonical hex quantity"),
            ("0xg", "exceeds 256 bits"),
            ("0xé", "exceeds 256 bits"),
        ] {
            assert_eq!(
                quantity(text, "chain ID").unwrap_err().message,
                format!("Ethereum chain ID {reason}")
            );
        }
        assert_eq!(
            quantity(&format!("0x1{}", "0".repeat(64)), "value")
                .unwrap_err()
                .message,
            "Ethereum value exceeds 256 bits"
        );
    }

    #[test]
    fn address_and_hash_fields_keep_alloy_syntax_and_contextual_errors() {
        for prefix in ["", "0x", "0X"] {
            let value = json!({
                "address": format!("{prefix}{}", "aB".repeat(20)),
                "hash": format!("{prefix}{}", "aB".repeat(32)),
            });
            let object = value.as_object().unwrap();
            assert_eq!(
                required_address(object, "address", "sender").unwrap(),
                Address::from_bytes([0xab; 20])
            );
            assert_eq!(
                optional_address(object, "address", "recipient").unwrap(),
                Some(Address::from_bytes([0xab; 20]))
            );
            assert_eq!(
                required_hash(object, "hash", "transaction hash").unwrap(),
                [0xab; 32]
            );
        }
        for invalid in ["0x", "0Xabcd", "0x0xabcd", "é", "zz"] {
            let value = json!({"address": invalid, "hash": invalid});
            let object = value.as_object().unwrap();
            assert_eq!(
                required_address(object, "address", "sender")
                    .unwrap_err()
                    .message,
                "Ethereum sender is not a 20-byte address"
            );
            assert_eq!(
                optional_address(object, "address", "recipient")
                    .unwrap_err()
                    .message,
                "Ethereum recipient is not a 20-byte address"
            );
            assert_eq!(
                required_hash(object, "hash", "transaction hash")
                    .unwrap_err()
                    .message,
                "Ethereum transaction hash is not a 32-byte hash"
            );
        }
        let object = Map::new();
        assert_eq!(
            optional_address(&object, "address", "recipient").unwrap(),
            None
        );
        assert_eq!(
            required_address(&object, "address", "sender")
                .unwrap_err()
                .message,
            "Ethereum sender is missing"
        );
        assert_eq!(
            required_hash(&object, "hash", "transaction hash")
                .unwrap_err()
                .message,
            "Ethereum transaction hash is missing"
        );
        let value = json!({"address": 1});
        assert_eq!(
            optional_address(value.as_object().unwrap(), "address", "recipient")
                .unwrap_err()
                .message,
            "Ethereum recipient must be an address or null"
        );
    }
}
