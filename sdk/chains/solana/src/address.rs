//! [`Address`]: a Solana account of one cluster, usable on its own or as a
//! [`base::Address`].

use std::fmt;

use anyhow::{Context as _, ensure};

use crate::Network;

/// A 32-byte Solana account of one cluster, with its canonical
/// [`base::Address`] (Base58). Off-curve (program-derived) addresses are
/// accounts too; whether value may be sent to one is a transfer rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    native: solana_address::Address,
    address: base::Address,
}

impl Address {
    /// `native` on `network`.
    #[must_use]
    pub fn new(native: solana_address::Address, network: &Network) -> Self {
        let address = base::Address::new(network.into(), native.to_string());
        Self { native, address }
    }

    /// `text` as an address of `network`: canonical Base58 of 32 bytes, the
    /// only textual form. The cluster is not in the text.
    ///
    /// # Errors
    ///
    /// For text that is not such an address.
    pub fn parse(text: &str, network: &Network) -> anyhow::Result<Self> {
        let native = text
            .parse::<solana_address::Address>()
            .ok()
            .context("Solana address must be Base58 of 32 bytes")?;
        ensure!(
            native.to_string() == text,
            "Solana address is not canonical Base58"
        );
        Ok(Self::new(native, network))
    }

    /// The `solana_address` address.
    #[must_use]
    pub const fn as_native(&self) -> &solana_address::Address {
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
