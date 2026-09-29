use base::{ChainName, ChainNetwork, ChainNetworks, NetworkKind};

use super::{Error, ErrorKind};

/// The chain family: the one chain key of metadata, indexing networks, and
/// persistence.
pub const CHAIN_NAME: ChainName = ChainName::from_static("ethereum");

pub(super) const MAINNET: ChainNetwork =
    ChainNetwork::new(CHAIN_NAME, "1", NetworkKind::Mainnet, "ETH");
const TESTNET: &[(&str, ChainNetwork)] = &[(
    "sepolia",
    ChainNetwork::new(CHAIN_NAME, "11155111", NetworkKind::Testnet, "ETH"),
)];

/// Well-known Ethereum networks and their chain IDs.
pub const NETWORKS: ChainNetworks = ChainNetworks::new(MAINNET, TESTNET);

/// An EVM chain's numeric identity and stable application network name.
///
/// The numeric ID is verified against RPC by `rpc::Client::connect`. The slug
/// names persisted networks and must stay stable across restarts; neither value
/// is derived from the other, because EVM chains are an open set.
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
    /// [`ErrorKind::InvalidConfiguration`].
    pub fn validate(&self) -> Result<(), Error> {
        if self.chain_id == 0 || self.slug.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum network needs a non-zero chain ID and a non-empty slug",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_uses_the_canonical_chain_key() {
        assert_eq!(NETWORKS.mainnet.name, CHAIN_NAME);
        assert!(
            NETWORKS
                .testnets
                .iter()
                .all(|(_, chain)| chain.name == CHAIN_NAME)
        );
    }

    #[test]
    fn validation_needs_a_chain_id_and_a_slug() {
        assert!(Network::new(31_337, "anvil").validate().is_ok());
        for invalid in [Network::new(0, "anvil"), Network::new(1, " ")] {
            let error = invalid.validate().unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        }
    }
}
