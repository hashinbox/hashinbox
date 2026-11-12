//! [`Network`]: an EVM network's identity, and the native currency.

use anyhow::ensure;
use base::Chain;

/// The native currency's name.
pub const ETH_NAME: &str = "Ethereum";
/// The native currency's ticker.
pub const ETH_TICKER: &str = "ETH";
/// Wei digits in one ether.
pub const ETH_DECIMALS: u32 = 18;

/// The chain family name.
pub const CHAIN_NAME: Chain = Chain::from_static("ethereum");

/// An EVM chain's numeric identity and stable application network name.
///
/// The chain ID is checked against the node by
/// [`Info`](crate::Info)'s `info()`. The slug names stored networks
/// and must stay stable across restarts; neither is derived from the other,
/// because EVM chains are an open set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    /// EIP-155 chain ID, checked against `eth_chainId`.
    pub chain_id: u64,
    /// Stable application name of the network, such as `mainnet` or `anvil`.
    pub slug: String,
}

impl Network {
    /// A network with `chain_id` named `slug`.
    #[must_use]
    pub fn new(chain_id: u64, slug: impl Into<String>) -> Self {
        Self {
            chain_id,
            slug: slug.into(),
        }
    }

    /// Rejects a zero chain ID or a blank slug.
    ///
    /// # Errors
    ///
    /// When either is missing.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.chain_id != 0 && !self.slug.trim().is_empty(),
            "Ethereum network needs a non-zero chain ID and a non-empty slug"
        );
        Ok(())
    }
}

/// The chain-neutral identity of this network: `(ethereum, slug)`.
impl From<&Network> for base::Network {
    fn from(network: &Network) -> Self {
        Self::new(CHAIN_NAME, network.slug.clone())
    }
}
