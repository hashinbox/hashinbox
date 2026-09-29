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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities_validate_syntax_before_numeric_width() {
        assert_eq!(parse_quantity_u64("0xffffffffffffffff"), Ok(u64::MAX));
        assert_eq!(
            parse_quantity_u64("0x10000000000000000"),
            Err("hex quantity exceeds u64")
        );
        for (text, message) in [
            ("0X1", "hex quantity has no 0x prefix"),
            ("0x", "hex quantity is empty"),
            ("0x0g", "hex quantity contains a leading zero"),
            ("0x_", "hex quantity contains invalid data"),
            ("0x1_0", "hex quantity contains invalid data"),
            ("0xé", "hex quantity contains invalid data"),
            ("0x0x1", "hex quantity contains a leading zero"),
            ("0x10000000000000000g", "hex quantity contains invalid data"),
        ] {
            assert_eq!(parse_quantity_u64(text), Err(message));
            assert_eq!(Wei::from_quantity(text), Err(message));
        }
    }

    #[test]
    fn rpc_transaction_id_keeps_mixed_case_bytes_and_method_errors() {
        let value = format!("0x{}", "aB".repeat(32));
        assert_eq!(
            TransactionId::from_rpc(&value, "eth_sendRawTransaction").unwrap(),
            TransactionId::from_bytes([0xab; 32])
        );
        for (text, reason) in [
            ("0X00", "hex data has no 0x prefix"),
            ("0xzz", "hex data has an invalid length"),
        ] {
            let error = TransactionId::from_rpc(text, "eth_getTransactionByHash").unwrap_err();
            assert_eq!(
                error.message,
                format!(
                    "Ethereum RPC eth_getTransactionByHash returned an invalid response: {reason}"
                )
            );
            assert!(!error.retryable);
        }
        let error =
            TransactionId::from_rpc(&format!("0x{}", "zz".repeat(32)), "eth_sendRawTransaction")
                .unwrap_err();
        assert_eq!(
            error.message,
            "Ethereum RPC eth_sendRawTransaction returned an invalid response: hex \
             data contains invalid data"
        );
        assert!(!error.retryable);
    }

    #[test]
    fn quantities_encode_zero_odd_nibbles_and_the_entire_256_bit_range() {
        for (value, expected) in [
            (Wei::ZERO, "0x0".to_owned()),
            (Wei::from_u128(15), "0xf".to_owned()),
            (Wei::from_u128(16), "0x10".to_owned()),
            (Wei::from_u128(0xabc), "0xabc".to_owned()),
            (Wei::from_u128(u128::MAX), format!("0x{}", "f".repeat(32))),
            (Wei::from_atomic([255; 32]), format!("0x{}", "f".repeat(64))),
        ] {
            assert_eq!(value.to_quantity(), expected);
            assert_eq!(Wei::from_quantity(&expected), Ok(value));
        }
        let mut value = [0; 32];
        value[0] = 1;
        value[31] = 0xab;
        assert_eq!(
            Wei::from_atomic(value).to_quantity(),
            format!("0x1{}ab", "00".repeat(30))
        );
    }

    #[test]
    fn variable_data_preserves_bytes_and_exact_validation_errors() {
        assert_eq!(parse_data("0x"), Ok(Vec::new()));
        assert_eq!(parse_data("0x000aBcFF"), Ok(vec![0, 10, 188, 255]));
        let every_byte: Vec<u8> = (0..=255).collect();
        assert_eq!(
            parse_data(&hex::encode_prefixed(&every_byte)),
            Ok(every_byte)
        );
        for (input, message) in [
            ("", "hex data has no 0x prefix"),
            ("0X00", "hex data has no 0x prefix"),
            ("0xg", "hex data has an invalid length"),
            ("0x€", "hex data has an invalid length"),
            ("0xé", "hex data contains invalid data"),
            ("0x0x", "hex data contains invalid data"),
            ("0x0x12", "hex data contains invalid data"),
            ("0x0X12", "hex data contains invalid data"),
            ("0x+1", "hex data contains invalid data"),
            ("0x 1", "hex data contains invalid data"),
            ("0xzz", "hex data contains invalid data"),
        ] {
            assert_eq!(parse_data(input), Err(message));
        }
    }

    #[test]
    fn quantities_preserve_big_endian_values_and_strict_wire_syntax() {
        for (encoded, expected) in [
            ("0x0", Wei::from_u128(0)),
            ("0xf", Wei::from_u128(15)),
            ("0xAbC", Wei::from_u128(0xabc)),
        ] {
            assert_eq!(Wei::from_quantity(encoded), Ok(expected));
        }
        assert_eq!(
            Wei::from_quantity(&format!("0x{}", "f".repeat(64))),
            Ok(Wei::from_atomic([255; 32]))
        );
        for (encoded, message) in [
            ("1", "hex quantity has no 0x prefix"),
            ("0x", "hex quantity is empty"),
            ("0x01", "hex quantity contains a leading zero"),
            ("0xg", "hex quantity contains invalid data"),
            ("0xé", "hex quantity contains invalid data"),
        ] {
            assert_eq!(Wei::from_quantity(encoded), Err(message));
        }
        assert_eq!(
            Wei::from_quantity(&format!("0x1{}", "0".repeat(64))),
            Err("hex quantity exceeds 256 bits")
        );
    }

    #[test]
    fn fixed_data_preserves_leading_zeroes_and_requires_exact_width() {
        assert_eq!(parse_fixed_data::<3>("0x000aBc"), Ok([0, 10, 188]));
        assert_eq!(parse_fixed_data::<0>("0x"), Ok([]));
        assert_eq!(
            parse_fixed_data::<2>("0xabc"),
            Err("hex data has an invalid length")
        );
        assert_eq!(
            parse_fixed_data::<2>("abcd"),
            Err("hex data has no 0x prefix")
        );
        assert_eq!(
            parse_fixed_data::<2>("0x0x12"),
            Err("hex data contains invalid data")
        );
        assert_eq!(
            parse_fixed_data::<2>("0x12zz"),
            Err("hex data contains invalid data")
        );
        assert_eq!(
            parse_fixed_data::<1>("0xé"),
            Err("hex data contains invalid data")
        );
        let mut hash = [0; 32];
        hash[30] = 0xab;
        hash[31] = 0xcd;
        assert_eq!(
            TransactionId::from_bytes(hash).to_string(),
            format!("0x{}abcd", "00".repeat(30))
        );
    }
}
