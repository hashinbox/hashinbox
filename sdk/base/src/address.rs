use std::fmt;

use crate::Network;

/// A chain-neutral address: canonical text inside one network.
///
/// `value` is always the chain's canonical text (Ethereum lowercase `0x`
/// hex, Bitcoin canonical text of its network, Solana Base58), so string
/// equality is address equality. Only a chain's `identity.rs` builds one from
/// a native address (`canonical`) and turns one back (`TryFrom<&Address>`);
/// human input is parsed by the chain's wallet factory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    /// The complete chain/network namespace in which `value` is canonical.
    pub network: Network,
    /// The canonical text.
    pub value: String,
}

impl Address {
    /// Takes the canonical text of `network`'s chain as the address's identity.
    ///
    /// Text, not anything printable: the caller states that what it hands over
    /// *is* the canonical form, which a chain writes as
    /// `native_address.to_string()` in its `identity.rs`. An `impl Display`
    /// parameter accepted an integer, and this constructor's own test built an
    /// address out of one.
    #[must_use]
    pub fn new(network: Network, value: impl Into<String>) -> Self {
        Self {
            network,
            value: value.into(),
        }
    }

    /// Whether this address is canonical in `network`.
    #[must_use]
    pub fn belongs_to(&self, network: &Network) -> bool {
        &self.network == network
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_uses_canonical_text_within_its_network() {
        let network = Network::new(crate::ChainName::new("test"), "mainnet");
        let address = Address::new(network.clone(), "canonical-text");

        assert_eq!(address.value, "canonical-text");
        assert_eq!(address.to_string(), "canonical-text");
        assert!(address.belongs_to(&network));
        assert!(!address.belongs_to(&Network::new(crate::ChainName::new("test"), "testnet")));
    }
}
