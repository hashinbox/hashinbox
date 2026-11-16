//! ERC-20 ABI: the calls a wallet makes and the `Transfer` event it indexes.

use std::{collections::BTreeSet, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolCall as _, SolEvent as _, sol};
use anyhow::anyhow;

sol! {
    interface Erc20 {
        event Transfer(address indexed from, address indexed to, uint256 value);
        function decimals() external view returns (uint8);
        function balanceOf(address account) external view returns (uint256);
        function transfer(address recipient, uint256 amount) external returns (bool);
    }
}

/// The `Transfer` event's topic: what `eth_getLogs` filters on.
pub(crate) const TRANSFER: B256 = Erc20::Transfer::SIGNATURE_HASH;

/// `transfer(to, amount)` calldata.
pub(crate) fn transfer(to: Address, amount: U256) -> Vec<u8> {
    Erc20::transferCall {
        recipient: to,
        amount,
    }
    .abi_encode()
}

/// `decimals()` calldata.
pub(crate) fn decimals() -> Vec<u8> {
    Erc20::decimalsCall {}.abi_encode()
}

/// The `uint8` a `decimals()` call returned.
pub(crate) fn decode_decimals(word: &[u8]) -> anyhow::Result<u8> {
    Erc20::decimalsCall::abi_decode_returns(word)
        .map_err(|_| anyhow!("invalid ERC-20 decimals() result"))
}

/// `balanceOf(account)` calldata.
pub(crate) fn balance_of(account: Address) -> Vec<u8> {
    Erc20::balanceOfCall { account }.abi_encode()
}

/// The `uint256` a `balanceOf()` call returned.
pub(crate) fn decode_balance(word: &[u8]) -> anyhow::Result<U256> {
    Erc20::balanceOfCall::abi_decode_returns(word)
        .map_err(|_| anyhow!("invalid ERC-20 balanceOf() result"))
}

/// `(from, to, amount)` of a log, when it is a `Transfer`; the zero address
/// is a mint's `from` and a burn's `to`.
pub(crate) fn transfer_log(topics: &[B256], data: &[u8]) -> Option<(Address, Address, U256)> {
    let [signature, from, to] = topics else {
        return None;
    };
    if *signature != TRANSFER || data.len() != 32 {
        return None;
    }
    Some((
        Address::from_word(*from),
        Address::from_word(*to),
        U256::from_be_slice(data),
    ))
}

/// The allowlisted ERC-20 contracts: the only tokens indexed and sent.
/// Cheap to clone; clones share the set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tokens(Arc<BTreeSet<Address>>);

impl Tokens {
    /// The tokens of `contracts`.
    #[must_use]
    pub fn new(contracts: impl IntoIterator<Item = Address>) -> Self {
        Self(Arc::new(contracts.into_iter().collect()))
    }

    /// Whether `contract` is allowlisted.
    #[must_use]
    pub fn contains(&self, contract: &Address) -> bool {
        self.0.contains(contract)
    }

    /// Every allowlisted contract.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Address> + '_ {
        self.0.iter().copied()
    }

    /// The contract of `asset`, when it is an allowlisted token; the asset's
    /// network is the caller's to check.
    #[must_use]
    pub fn get(&self, asset: &base::Asset) -> Option<Address> {
        asset
            .id
            .parse()
            .ok()
            .filter(|contract| self.contains(contract))
    }
}
