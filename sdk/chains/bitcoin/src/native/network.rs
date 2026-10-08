use base::{ChainName, ChainNetwork, ChainNetworks, NetworkKind};

/// The chain family: the one chain key of metadata, indexing networks, and
/// persistence.
pub const CHAIN_NAME: ChainName = ChainName::from_static("bitcoin");

pub(super) const MAINNET: ChainNetwork = Network::Mainnet.metadata();
const TESTNET: &[(&str, ChainNetwork)] = &[
    ("testnet3", Network::Testnet3.metadata()),
    ("testnet4", Network::Testnet4.metadata()),
    ("signet", Network::Signet.metadata()),
    ("regtest", Network::Regtest.metadata()),
];

/// Well-known Bitcoin networks.
pub const NETWORKS: ChainNetworks = ChainNetworks::new(MAINNET, TESTNET);

/// A Bitcoin network: a closed set, verified against Bitcoin Core's chain name
/// by `rpc::Client::connect`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// Bitcoin mainnet.
    Mainnet,
    /// The third public test network.
    Testnet3,
    /// The fourth public test network.
    Testnet4,
    /// The default signet.
    Signet,
    /// A local regression-test network.
    Regtest,
}

impl Network {
    /// Every network, mainnet first.
    pub const ALL: [Self; 5] = [
        Self::Mainnet,
        Self::Testnet3,
        Self::Testnet4,
        Self::Signet,
        Self::Regtest,
    ];

    /// Stable application name: `mainnet`, `testnet3`, `testnet4`, `signet`, `regtest`.
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

    /// The network whose [`Network::slug`] is `slug`.
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|network| network.slug() == slug)
    }

    /// The chain name Bitcoin Core reports in `getblockchaininfo`.
    #[must_use]
    pub const fn core_chain(self) -> &'static str {
        match self {
            Self::Mainnet => "main",
            Self::Testnet3 => "test",
            Self::Testnet4 => "testnet4",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    pub(crate) fn from_core_chain(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|network| network.core_chain() == name)
    }

    pub(crate) const fn native(self) -> bitcoin::Network {
        match self {
            Self::Mainnet => bitcoin::Network::Bitcoin,
            Self::Testnet3 => bitcoin::Network::Testnet,
            Self::Testnet4 => bitcoin::Network::Testnet4,
            Self::Signet => bitcoin::Network::Signet,
            Self::Regtest => bitcoin::Network::Regtest,
        }
    }

    const fn metadata(self) -> ChainNetwork {
        let (native_id, kind) = match self {
            Self::Mainnet => ("bitcoin-mainnet", NetworkKind::Mainnet),
            Self::Testnet3 => ("bitcoin-testnet3", NetworkKind::Testnet),
            Self::Testnet4 => ("bitcoin-testnet4", NetworkKind::Testnet),
            Self::Signet => ("bitcoin-signet", NetworkKind::Testnet),
            Self::Regtest => ("bitcoin-regtest", NetworkKind::Testnet),
        };
        ChainNetwork::new(CHAIN_NAME, native_id, kind, "BTC")
    }
}
