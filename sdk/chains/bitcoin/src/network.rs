//! [`Network`]: the supported Bitcoin networks and the native currency.

use base::Chain;

/// The native currency's name.
pub const BTC_NAME: &str = "Bitcoin";
/// The native currency's ticker.
pub const BTC_TICKER: &str = "BTC";
/// Satoshi digits in one bitcoin.
pub const BTC_DECIMALS: u32 = 8;

/// The chain family name.
pub const CHAIN_NAME: Chain = Chain::from_static("bitcoin");

/// A supported Bitcoin network.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// Bitcoin mainnet.
    Mainnet,
    /// Testnet 3.
    Testnet3,
    /// Testnet 4.
    Testnet4,
    /// Signet.
    Signet,
    /// Regtest.
    Regtest,
}

impl Network {
    const ALL: [Self; 5] = [
        Self::Mainnet,
        Self::Testnet3,
        Self::Testnet4,
        Self::Signet,
        Self::Regtest,
    ];

    /// The canonical network name.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet3 => "testnet3",
            Self::Testnet4 => "testnet4",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    /// The network Bitcoin Core's `getblockchaininfo` names `chain`.
    #[must_use]
    pub fn from_core_chain(chain: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|network| network.native().to_core_arg() == chain)
    }

    /// The `bitcoin` crate's network.
    #[must_use]
    pub const fn native(self) -> bitcoin::Network {
        match self {
            Self::Mainnet => bitcoin::Network::Bitcoin,
            Self::Testnet3 => bitcoin::Network::Testnet,
            Self::Testnet4 => bitcoin::Network::Testnet4,
            Self::Signet => bitcoin::Network::Signet,
            Self::Regtest => bitcoin::Network::Regtest,
        }
    }
}

/// The chain-neutral identity of this network: `(bitcoin, slug)`.
impl From<Network> for base::Network {
    fn from(network: Network) -> Self {
        Self::new(CHAIN_NAME, network.slug())
    }
}
