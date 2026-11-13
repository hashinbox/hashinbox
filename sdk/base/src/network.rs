use crate::Chain;

/// One exact `(chain, network)` namespace; every indexed identity belongs to one.
///
/// The chain-neutral counterpart of a chain crate's native network type: each
/// chain implements `From<&NativeNetwork> for Network`, the only conversion
/// from a native network to the SDK's stored identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Network {
    /// The chain family.
    pub chain: Chain,
    /// Chain-owned canonical network name, such as mainnet, sepolia, or regtest.
    pub name: String,
}

impl Network {
    /// Names `name` within `chain`; the chain crate owns the canonical spelling.
    #[must_use]
    pub fn new(chain: Chain, name: impl Into<String>) -> Self {
        Self {
            chain,
            name: name.into(),
        }
    }
}

/// Lets a constructor take `impl Into<Network>` and accept a borrowed
/// network as readily as a chain's native one.
impl From<&Network> for Network {
    fn from(network: &Network) -> Self {
        network.clone()
    }
}

/// Anything that lives on exactly one network.
pub trait Networked {
    /// The network.
    fn network(&self) -> &Network;
}

/// A network is on itself, so generic code over [`Networked`] takes a plain
/// [`Network`] too.
impl Networked for Network {
    fn network(&self) -> &Network {
        self
    }
}
