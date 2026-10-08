//! Strict JSON-RPC QUANTITY and DATA codecs.

use alloy_primitives::{U256, hex};

use crate::native::{Error, TransactionId, Wei};

/// A `u64` QUANTITY: `0x`, no leading zero, ASCII hex.
pub(crate) fn parse_quantity_u64(value: &str) -> Result<u64, &'static str> {
    let digits = quantity_digits(value)?;
    u64::from_str_radix(digits, 16).map_err(|_| "hex quantity exceeds u64")
}

impl Wei {
    pub(crate) fn to_quantity(&self) -> String {
        format!("{:#x}", self.u256())
    }

    pub(crate) fn from_quantity(value: &str) -> Result<Self, &'static str> {
        let digits = quantity_digits(value)?;
        if digits.len() > 64 {
            return Err("hex quantity exceeds 256 bits");
        }
        U256::from_str_radix(digits, 16)
            .map(Self::from_u256)
            .map_err(|_| "hex data contains invalid data")
    }
}

/// The digits of a QUANTITY, checked for grammar before any width check.
fn quantity_digits(value: &str) -> Result<&str, &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex quantity has no 0x prefix")?;
    if digits.is_empty() {
        return Err("hex quantity is empty");
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err("hex quantity contains a leading zero");
    }
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("hex quantity contains invalid data");
    }
    Ok(digits)
}

/// Exactly `N` bytes of DATA with a lowercase `0x` prefix.
pub(crate) fn parse_fixed_data<const N: usize>(value: &str) -> Result<[u8; N], &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex data has no 0x prefix")?;
    if digits.len() != N * 2 {
        return Err("hex data has an invalid length");
    }
    hex::decode_to_array(digits).map_err(|_| "hex data contains invalid data")
}

/// Any number of bytes of DATA with a lowercase `0x` prefix.
pub(crate) fn parse_data(value: &str) -> Result<Vec<u8>, &'static str> {
    let digits = value
        .strip_prefix("0x")
        .ok_or("hex data has no 0x prefix")?;
    if digits.len() % 2 != 0 {
        return Err("hex data has an invalid length");
    }
    // The decoder removes one prefix, so pass the original input to keep a
    // second prefix invalid rather than accepting it as another optional prefix.
    hex::decode(value).map_err(|_| "hex data contains invalid data")
}

impl TransactionId {
    pub(crate) fn from_rpc(value: &str, method: &str) -> Result<Self, Error> {
        parse_fixed_data::<32>(value)
            .map(Self::from_bytes)
            .map_err(|message| Error::invalid_response(method, message))
    }
}
