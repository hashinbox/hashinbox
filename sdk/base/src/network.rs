use crate::ChainName;

/// Coarse environment classification shared by every supported protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetworkKind {
    /// Real value moves here.
    Mainnet,
    /// Any test or local network.
    Testnet,
}

/// One exact `(chain, network)` namespace; every indexed identity belongs to one.
///
/// The chain-neutral counterpart of a chain crate's native network type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Network {
    /// The chain family.
    pub chain: ChainName,
    /// Chain-owned canonical network name, such as mainnet, sepolia, or regtest.
    pub name: String,
}

impl Network {
    /// Names `name` within `chain`; the chain crate owns the canonical spelling.
    #[must_use]
    pub fn new(chain: ChainName, name: impl Into<String>) -> Self {
        Self {
            chain,
            name: name.into(),
        }
    }
}

/// A value that names exactly one `(chain, network)` namespace.
///
/// Implemented by every chain's native network type in its `identity.rs`, and
/// reflexively by [`Network`] itself, so a constructor takes `&impl Networked`
/// and accepts either. It is the only conversion from a native network to the
/// SDK's stored identity; nothing else computes a network.
pub trait Networked {
    /// This value's chain-neutral identity: what an index is partitioned by,
    /// the namespace of every [`crate::Address`] and [`crate::TransactionRef`],
    /// and the key a repository is opened with.
    fn canonical(&self) -> Network;
}

impl Networked for Network {
    /// A network is its own canonical identity.
    fn canonical(&self) -> Network {
        self.clone()
    }
}
