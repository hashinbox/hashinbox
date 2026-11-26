//! [`Account`]: a Tron account's 20 bytes and its `T…` text; [`Address`]:
//! one of a network, usable on its own or as a [`base::Address`].

use std::{fmt, str::FromStr};

use alloy_primitives::{hex, keccak256};
use anyhow::{Context as _, bail, ensure};
use crypto::Secp256k1Ecdsa;

use crate::Network;

/// The byte every Tron address starts with: its hex form's `41`.
const PREFIX: u8 = 0x41;

/// A Tron account or contract: the last 20 bytes of the Keccak-256 of its
/// key (an Ethereum address's bytes), written as Base58Check of
/// `0x41 ‖ bytes` (`T…`), its only canonical text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Account(pub [u8; 20]);

impl Account {
    /// The account `key` signs for.
    #[must_use]
    pub fn from_key(key: &Secp256k1Ecdsa) -> Self {
        let hash = keccak256(key.raw_public_key());
        let mut bytes = [0; 20];
        bytes.copy_from_slice(&hash[12..]);
        Self(bytes)
    }

    /// `41` and 40 hex digits, any case: the form the node writes.
    ///
    /// # Errors
    ///
    /// For text that is not.
    pub fn from_hex(text: &str) -> anyhow::Result<Self> {
        let bytes = hex::decode(text).context("Tron hex address is not hex")?;
        Self::from_prefixed(&bytes)
    }

    /// `0x41 ‖ 20 bytes`, as a transaction carries it.
    ///
    /// # Errors
    ///
    /// For any other bytes.
    pub fn from_prefixed(bytes: &[u8]) -> anyhow::Result<Self> {
        let Some((&PREFIX, rest)) = bytes.split_first() else {
            bail!("Tron address must start with 0x41");
        };
        let account = rest.try_into().context("Tron address must be 21 bytes")?;
        Ok(Self(account))
    }

    /// `0x41 ‖ 20 bytes`, as a transaction carries it.
    #[must_use]
    pub fn prefixed(&self) -> [u8; 21] {
        let mut bytes = [PREFIX; 21];
        bytes[1..].copy_from_slice(&self.0);
        bytes
    }

    /// The 32-byte ABI word: the 20 bytes left-padded, as a TRC-20 call and
    /// its `Transfer` topics carry it.
    #[must_use]
    pub fn word(&self) -> [u8; 32] {
        let mut word = [0; 32];
        word[12..].copy_from_slice(&self.0);
        word
    }
}

/// Base58Check of `0x41 ‖ bytes` only.
impl FromStr for Account {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let bytes = crypto::base58::decode(text).context("Tron address is not Base58")?;
        ensure!(bytes.len() == 25, "Tron address must be 25 bytes");
        let (payload, check) = bytes.split_at(21);
        ensure!(
            checksum(payload) == check,
            "Tron address has an invalid checksum"
        );
        Self::from_prefixed(payload)
    }
}

impl fmt::Display for Account {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut bytes = self.prefixed().to_vec();
        bytes.extend(checksum(&bytes));
        formatter.write_str(&crypto::base58::encode(&bytes))
    }
}

/// The first 4 bytes of the double SHA-256 of `payload`.
fn checksum(payload: &[u8]) -> [u8; 4] {
    let hash = crypto::sha256(&crypto::sha256(payload));
    [hash[0], hash[1], hash[2], hash[3]]
}

/// A Tron account of one network, with its canonical [`base::Address`]
/// (its `T…` text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    native: Account,
    address: base::Address,
}

impl Address {
    /// `native` on `network`.
    #[must_use]
    pub fn new(native: Account, network: &Network) -> Self {
        let address = base::Address::new(network.into(), native.to_string());
        Self { native, address }
    }

    /// `text` as an address of `network`: `T…` Base58Check, or `41` and 40
    /// hex digits. The network is not in the text: every Tron network
    /// shares one encoding.
    ///
    /// # Errors
    ///
    /// For text that is neither.
    pub fn parse(text: &str, network: &Network) -> anyhow::Result<Self> {
        let native = if text.len() == 42 {
            Account::from_hex(text)?
        } else {
            text.parse()?
        };
        Ok(Self::new(native, network))
    }

    /// The account.
    #[must_use]
    pub const fn as_native(&self) -> &Account {
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
