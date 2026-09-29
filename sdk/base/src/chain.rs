use std::{borrow::Cow, fmt};

use crate::NetworkKind;

/// Chain family identity, such as `bitcoin`, `ethereum`, or `solana`.
///
/// Owned form of [`ChainNetwork::name`]; persisted and compared as its exact text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainName(Cow<'static, str>);

impl ChainName {
    /// Lets a chain crate declare its family as a `const`.
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// Decodes a stored or transported family name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(Cow::Owned(name.into()))
    }

    /// The exact family text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChainName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Static metadata of one well-known network of a chain family.
///
/// One value is one *network*, not one chain: `bitcoin` mainnet and
/// `bitcoin` regtest are two of these sharing one [`ChainName`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChainNetwork {
    /// The chain family, the same value as the chain crate's `CHAIN_NAME`.
    pub name: ChainName,
    /// The network's own identifier: a chain ID, cluster, or network name.
    pub native_id: &'static str,
    /// Mainnet or testnet.
    pub kind: NetworkKind,
    /// Display symbol of the native currency on this network.
    pub ticker: &'static str,
}

impl ChainNetwork {
    /// Declares a network as a `const`.
    #[must_use]
    pub const fn new(
        name: ChainName,
        native_id: &'static str,
        kind: NetworkKind,
        ticker: &'static str,
    ) -> Self {
        Self {
            name,
            native_id,
            kind,
            ticker,
        }
    }

    /// Whether this network moves real value.
    #[must_use]
    pub const fn is_mainnet(&self) -> bool {
        matches!(self.kind, NetworkKind::Mainnet)
    }
}

impl fmt::Display for ChainNetwork {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name, self.native_id)
    }
}

/// Every well-known network of one chain family: its mainnet and its named
/// test networks.
#[derive(Clone, Debug)]
pub struct ChainNetworks {
    /// The production network.
    pub mainnet: ChainNetwork,
    /// Named test networks in declaration order; names are chain-owned.
    pub testnets: &'static [(&'static str, ChainNetwork)],
}

impl ChainNetworks {
    /// Declares a chain's networks as a `const`.
    #[must_use]
    pub const fn new(
        mainnet: ChainNetwork,
        testnets: &'static [(&'static str, ChainNetwork)],
    ) -> Self {
        Self { mainnet, testnets }
    }

    /// Finds a test network by name, ignoring ASCII case.
    #[must_use]
    pub fn testnet(&self, name: &str) -> Option<&'static ChainNetwork> {
        self.testnets
            .iter()
            .find_map(|(key, chain)| key.eq_ignore_ascii_case(name).then_some(chain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: ChainName = ChainName::from_static("one");
    const MAINNET: ChainNetwork = ChainNetwork::new(ONE, "one", NetworkKind::Mainnet, "ONE");
    const TESTNET: &[(&str, ChainNetwork)] = &[(
        "local",
        ChainNetwork::new(ONE, "one-test", NetworkKind::Testnet, "ONE"),
    )];

    #[test]
    fn chain_name_compares_by_text_regardless_of_storage() {
        const TEST: ChainName = ChainName::from_static("test");

        assert_eq!(TEST, ChainName::new("test"));
        assert_eq!(TEST.as_str(), "test");
        assert_eq!(ChainName::new("test").to_string(), "test");
    }

    #[test]
    fn networks_expose_mainnet_and_named_testnets_directly() {
        let chains = ChainNetworks::new(MAINNET, TESTNET);
        assert_eq!(chains.mainnet, MAINNET);
        assert!(chains.mainnet.is_mainnet());
        assert_eq!(chains.mainnet.to_string(), "one (one)");
        assert_eq!(chains.testnets, TESTNET);
        assert_eq!(chains.testnet("LOCAL"), Some(&TESTNET[0].1));
        assert_eq!(chains.testnet("missing"), None);
    }
}
