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
