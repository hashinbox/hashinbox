//! [`Address`]: an Ethereum account or contract of one network, usable on its
//! own or as a [`base::Address`].

use std::fmt;

use alloy_primitives::{hex, keccak256};
use anyhow::{bail, ensure};
use crypto::Secp256k1Ecdsa;

use crate::Network;

/// A 20-byte Ethereum address of one network, with its canonical
/// [`base::Address`] (lowercase `0x` hex).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    native: alloy_primitives::Address,
    address: base::Address,
}

impl Address {
    /// `native` on `network`.
    #[must_use]
    pub fn new(native: alloy_primitives::Address, network: &Network) -> Self {
        let address = base::Address::new(network.into(), hex::encode_prefixed(native));
        Self { native, address }
    }

    /// The account `key` signs for: the last 20 bytes of the Keccak-256 of
    /// its uncompressed public key.
    #[must_use]
    pub fn from_key(key: &Secp256k1Ecdsa, network: &Network) -> Self {
        let hash = keccak256(key.raw_public_key());
        Self::new(alloy_primitives::Address::from_slice(&hash[12..]), network)
    }

    /// `text` as an address of `network`: `0x` and 40 hex digits, all lower,
    /// all upper, or mixed case with a valid EIP-55 checksum. The network is
    /// not in the text: every EVM chain shares one encoding.
    ///
    /// # Errors
    ///
    /// For text that is not such an address.
    pub fn parse(text: &str, network: &Network) -> anyhow::Result<Self> {
        let Some(digits) = text.strip_prefix("0x") else {
            bail!("Ethereum address is missing its 0x prefix");
        };
        ensure!(
            digits.len() == 40,
            "Ethereum address must contain exactly 20 bytes"
        );
        let mut bytes = [0_u8; 20];
        hex::decode_to_slice(digits, &mut bytes)
            .map_err(|_| anyhow::anyhow!("Ethereum address contains invalid hexadecimal"))?;
        let native = alloy_primitives::Address::from(bytes);
        let mixed = digits.chars().any(|c| c.is_ascii_lowercase())
            && digits.chars().any(|c| c.is_ascii_uppercase());
        ensure!(
            !mixed || native.to_checksum(None) == text,
            "Ethereum address has an invalid EIP-55 checksum"
        );
        Ok(Self::new(native, network))
    }

    /// The `alloy` address.
    #[must_use]
    pub const fn as_native(&self) -> &alloy_primitives::Address {
        &self.native
    }
}

impl base::Addressed for Address {
    fn address(&self) -> &base::Address {
        &self.address
    }
}

impl base::Networked for Address {
    fn network(&self) -> &base::Network {
        &self.address.network
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.address.value)
    }
}

impl From<Address> for base::Address {
    fn from(address: Address) -> Self {
        address.address
    }
}
