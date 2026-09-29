//! ERC-20 ABI: calls the wallet makes and the `Transfer` event it indexes.

use alloy_primitives::Address as AlloyAddress;
use alloy_sol_types::{SolCall, sol};
use base::MovementKind;

use super::{
    Address, AssetKind, TransactionId, Wei,
    block::{Log, Movement},
    transaction::Transfer,
};

/// `keccak256("Transfer(address,address,uint256)")`.
pub const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];

sol! {
    interface Erc20 {
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
        function transfer(address recipient, uint256 amount) external returns (bool);
    }
}

/// `balanceOf(address)` calldata.
pub(crate) fn balance_of(address: &Address) -> Vec<u8> {
    Erc20::balanceOfCall {
        account: AlloyAddress::from(*address.as_bytes()),
    }
    .abi_encode()
}

/// `decimals()` calldata.
pub(crate) fn decimals() -> Vec<u8> {
    Erc20::decimalsCall {}.abi_encode()
}

impl Transfer {
    /// The transaction calldata: empty for ether, `transfer(..)` for ERC-20.
    #[must_use]
    pub fn input(&self) -> Vec<u8> {
        match self.asset {
            AssetKind::Native => Vec::new(),
            AssetKind::Erc20(_) => Erc20::transferCall {
                recipient: AlloyAddress::from(*self.to.as_bytes()),
                amount: self.amount.u256(),
            }
            .abi_encode(),
        }
    }
}

impl Wei {
    pub(crate) fn from_erc20_balance(word: &[u8]) -> Result<Self, &'static str> {
        let value = Erc20::balanceOfCall::abi_decode_returns_validate(word)
            .map_err(|_| "invalid balanceOf ABI word")?;
        if Erc20::balanceOfCall::abi_encode_returns(&value) != word {
            return Err("non-canonical balanceOf ABI word");
        }
        Ok(Self::from_u256(value))
    }
}

/// Strict `decimals()` return: one canonical `uint8` word.
pub(crate) fn decode_decimals(word: &[u8]) -> Result<u8, &'static str> {
    let value = Erc20::decimalsCall::abi_decode_returns_validate(word)
        .map_err(|_| "invalid decimals ABI word")?;
    if Erc20::decimalsCall::abi_encode_returns(&value) != word {
        return Err("non-canonical decimals ABI word");
    }
    Ok(value)
}

/// Strict `transfer(..)` return: one canonical `bool` word.
pub(crate) fn decode_transfer(word: &[u8]) -> Result<bool, &'static str> {
    let value = Erc20::transferCall::abi_decode_returns_validate(word)
        .map_err(|_| "invalid transfer ABI word")?;
    if Erc20::transferCall::abi_encode_returns(&value) != word {
        return Err("non-canonical transfer ABI word");
    }
    Ok(value)
}

impl Log {
    /// The ERC-20 movement this log records, if it is a well-formed `Transfer`.
    pub(crate) fn movement(&self, transaction: &TransactionId) -> Option<Movement> {
        let [signature, from, to] = self.topics.as_slice() else {
            return None;
        };
        if *signature != TRANSFER_TOPIC
            || self.data.len() != 32
            || from[..12] != [0; 12]
            || to[..12] != [0; 12]
        {
            return None;
        }
        let from = Address::from_bytes(from[12..].try_into().ok()?);
        let to = Address::from_bytes(to[12..].try_into().ok()?);
        let kind = match (from.is_zero(), to.is_zero()) {
            (true, true) => return None,
            (true, false) => MovementKind::Mint,
            (false, true) => MovementKind::Burn,
            (false, false) => MovementKind::Transfer,
        };
        Some(Movement {
            id: format!("{transaction}:{}", self.index),
            kind,
            asset: AssetKind::Erc20(self.address.clone()),
            amount: Wei::from_atomic(self.data.as_slice().try_into().ok()?),
            from: (!from.is_zero()).then_some(from),
            to: (!to.is_zero()).then_some(to),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_calls_match_canonical_erc20_abi() {
        assert_eq!(
            balance_of(&Address::from_bytes([0x11; 20])),
            [hex("70a08231"), vec![0; 12], vec![0x11; 20]].concat()
        );
        assert_eq!(decimals(), hex("313ce567"));
        assert_eq!(
            Transfer {
                asset: AssetKind::Erc20(Address::from_bytes([2; 20])),
                to: Address::from_bytes([0x22; 20]),
                amount: Wei::from_u128(7)
            }
            .input(),
            [
                hex("a9059cbb"),
                vec![0; 12],
                vec![0x22; 20],
                vec![0; 31],
                vec![7],
            ]
            .concat()
        );
    }

    #[test]
    fn transfer_input_preserves_all_amount_bits_and_native_input_is_empty() {
        for amount in [Wei::ZERO, Wei::from_atomic([255; 32])] {
            let request = Transfer {
                asset: AssetKind::Erc20(Address::from_bytes([2; 20])),
                to: Address::from_bytes([0xab; 20]),
                amount: amount.clone(),
            };
            assert_eq!(
                request.input(),
                [
                    hex("a9059cbb"),
                    vec![0; 12],
                    vec![0xab; 20],
                    amount.units().to_vec()
                ]
                .concat()
            );
        }
        let native = Transfer {
            asset: AssetKind::Native,
            to: Address::from_bytes([2; 20]),
            amount: Wei::from_atomic([255; 32]),
        };
        assert!(native.input().is_empty());
    }

    #[test]
    fn typed_results_require_canonical_words() {
        let mut amount = [0_u8; 32];
        amount[31] = 7;
        assert_eq!(Wei::from_erc20_balance(&amount), Ok(Wei::from_u128(7)));

        let mut decimals = [0_u8; 32];
        decimals[31] = 6;
        assert_eq!(decode_decimals(&decimals), Ok(6));

        let mut success = [0_u8; 32];
        success[31] = 1;
        assert_eq!(decode_transfer(&success), Ok(true));
        assert!(decode_transfer(&[]).is_err());
        assert!(decode_transfer(&[0; 31]).is_err());
        assert!(decode_transfer(&[2; 32]).is_err());
    }

    #[test]
    fn return_decoders_reject_truncation_and_trailing_words() {
        for length in [0, 31, 33, 64] {
            let bytes = vec![0; length];
            assert!(Wei::from_erc20_balance(&bytes).is_err());
            assert!(decode_decimals(&bytes).is_err());
            assert!(decode_transfer(&bytes).is_err());
        }
        assert_eq!(
            Wei::from_erc20_balance(&[255; 32]),
            Ok(Wei::from_atomic([255; 32]))
        );
        assert_eq!(decode_transfer(&[0; 32]), Ok(false));
        let mut word = [0; 32];
        word[31] = 255;
        assert_eq!(decode_decimals(&word), Ok(255));
        assert!(decode_transfer(&word).is_err());
        word[0] = 1;
        assert!(decode_decimals(&word).is_err());
        assert!(decode_transfer(&word).is_err());
    }

    #[test]
    fn balance_decoder_preserves_zero_and_distinguishes_invalid_from_noncanonical_words() {
        assert_eq!(Wei::from_erc20_balance(&[0; 32]), Ok(Wei::ZERO));
        for (length, message) in [
            (0, "invalid balanceOf ABI word"),
            (31, "invalid balanceOf ABI word"),
            (33, "non-canonical balanceOf ABI word"),
            (64, "non-canonical balanceOf ABI word"),
        ] {
            assert_eq!(Wei::from_erc20_balance(&vec![0; length]), Err(message));
        }
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("test ABI hex must be UTF-8");
                u8::from_str_radix(pair, 16).expect("test ABI hex must be valid")
            })
            .collect()
    }
}
