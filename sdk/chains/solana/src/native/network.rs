use std::{fmt, str::FromStr};

use base::{ChainName, ChainNetwork, ChainNetworks, NetworkKind};
use solana_hash::Hash;

use super::{Error, ErrorKind};

/// The chain family: the one chain key of metadata, indexing networks, and
/// persistence.
pub const CHAIN_NAME: ChainName = ChainName::from_static("solana");

pub(super) const MAINNET: ChainNetwork =
    ChainNetwork::new(CHAIN_NAME, "mainnet-beta", NetworkKind::Mainnet, "SOL");
const TESTNET: &[(&str, ChainNetwork)] = &[
    (
        "devnet",
        ChainNetwork::new(CHAIN_NAME, "devnet", NetworkKind::Testnet, "SOL"),
    ),
    (
        "testnet",
        ChainNetwork::new(CHAIN_NAME, "testnet", NetworkKind::Testnet, "SOL"),
    ),
];

/// Well-known Solana clusters.
pub const NETWORKS: ChainNetworks = ChainNetworks::new(MAINNET, TESTNET);

/// A Solana cluster: a stable application name and the genesis hash that
/// identifies it.
///
/// The genesis hash is verified against RPC by `rpc::Client::connect`. The
/// slug names persisted networks and must stay stable across restarts; neither
/// value is derived from the other, because clusters are an open set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    slug: String,
    genesis_hash: GenesisHash,
}

impl Network {
    /// A cluster named `slug` whose first block hashes to `genesis_hash`.
    #[must_use]
    pub fn new(slug: impl Into<String>, genesis_hash: GenesisHash) -> Self {
        Self {
            slug: slug.into(),
            genesis_hash,
        }
    }

    /// Stable application name of the cluster, such as `mainnet-beta` or `localnet`.
    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The genesis hash `rpc::Client::connect` requires the node to report.
    #[must_use]
    pub const fn genesis_hash(&self) -> &GenesisHash {
        &self.genesis_hash
    }

    /// Rejects a blank slug.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`].
    pub fn validate(&self) -> Result<(), Error> {
        if self.slug.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana network slug must not be empty",
            ));
        }
        Ok(())
    }
}

/// The hash of a cluster's genesis block: its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenesisHash(Hash);

impl FromStr for GenesisHash {
    type Err = Error;

    /// Accepts only canonical Base58.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana genesis hash must be canonical Base58",
            )
        };
        let hash = Hash::from_str(input).map_err(|_| invalid())?;
        if hash.to_string() != input {
            return Err(invalid());
        }
        Ok(Self(hash))
    }
}

impl fmt::Display for GenesisHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
