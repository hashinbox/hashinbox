//! [`Network`]: a Solana cluster's identity, and the native currency.

use std::{fmt, str::FromStr};

use anyhow::{Context as _, ensure};
use base::Chain;
use solana_hash::Hash;

/// The native currency's name.
pub const SOL_NAME: &str = "Solana";
/// The native currency's ticker.
pub const SOL_TICKER: &str = "SOL";
/// Lamport digits in one SOL.
pub const SOL_DECIMALS: u32 = 9;

/// The chain family name.
pub const CHAIN_NAME: Chain = Chain::from_static("solana");

/// A Solana cluster: the genesis hash every node must report and its stable
/// application name. Neither is derived from the other: a local validator has
/// its own genesis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    /// The cluster's genesis hash, checked against `getGenesisHash`.
    pub genesis_hash: GenesisHash,
    /// Stable application name of the cluster, such as `mainnet-beta`.
    pub slug: String,
}

impl Network {
    /// The cluster of `genesis_hash` named `slug`.
    #[must_use]
    pub fn new(genesis_hash: GenesisHash, slug: impl Into<String>) -> Self {
        Self {
            genesis_hash,
            slug: slug.into(),
        }
    }

    /// Rejects a blank slug.
    ///
    /// # Errors
    ///
    /// When the slug is blank.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.slug.trim().is_empty(),
            "Solana network needs a non-empty slug"
        );
        Ok(())
    }
}

/// The chain-neutral identity of this cluster: `(solana, slug)`.
impl From<&Network> for base::Network {
    fn from(network: &Network) -> Self {
        Self::new(CHAIN_NAME, network.slug.clone())
    }
}

/// The hash of a cluster's genesis block: its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenesisHash(Hash);

impl FromStr for GenesisHash {
    type Err = anyhow::Error;

    /// Accepts only canonical Base58 of 32 bytes.
    fn from_str(text: &str) -> anyhow::Result<Self> {
        let hash = Hash::from_str(text)
            .ok()
            .context("Solana genesis hash must be Base58 of 32 bytes")?;
        ensure!(
            hash.to_string() == text,
            "Solana genesis hash is not canonical Base58"
        );
        Ok(Self(hash))
    }
}

impl fmt::Display for GenesisHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> serde::Deserialize<'de> for GenesisHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}
