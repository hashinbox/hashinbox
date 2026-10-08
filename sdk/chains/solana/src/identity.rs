//! Native Solana identities in their chain-neutral (canonical) forms.

use base::{AssetId, Networked, TransactionRef};

use crate::native::{Address, AssetKind, CHAIN_NAME, Error, ErrorKind, Network, TransactionId};

/// The chain-neutral identity of this cluster: `(solana, slug)`.
impl Networked for Network {
    fn canonical(&self) -> base::Network {
        base::Network::new(CHAIN_NAME, self.slug())
    }
}

impl Address {
    /// This address inside `network`: canonical Base58, as indexing,
    /// wallets, and the API carry it. The address is the same on every
    /// cluster.
    #[must_use]
    pub fn canonical(&self, network: &impl Networked) -> base::Address {
        base::Address::new(network.canonical(), self.to_string())
    }
}

/// Strict: a Solana network with a network name, and exactly the canonical
/// Base58 of 32 bytes. Anything read back from storage, filters, or other
/// wallets goes through here. A Solana address names no cluster, so any
/// Solana network accepts it. User input goes through
/// `WalletFactory::parse_address`.
impl TryFrom<&base::Address> for Address {
    type Error = Error;

    fn try_from(address: &base::Address) -> Result<Self, Self::Error> {
        if address.network.chain != CHAIN_NAME || address.network.name.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidAddress,
                "address does not belong to a Solana network",
            ));
        }
        address.value.parse()
    }
}

impl TransactionId {
    /// This transaction inside `network`, as indexing and history store it.
    #[must_use]
    pub fn canonical(&self, network: &impl Networked) -> TransactionRef {
        TransactionRef::new(network.canonical(), self.to_string())
    }
}

impl From<&TransactionId> for base::TransactionId {
    fn from(id: &TransactionId) -> Self {
        Self::new(id.to_string())
    }
}

impl From<TransactionId> for base::TransactionId {
    fn from(id: TransactionId) -> Self {
        Self::from(&id)
    }
}

impl AssetKind {
    /// `solana/native`, or `solana/<canonical Base58 mint>`.
    #[must_use]
    pub fn id(&self) -> AssetId {
        match self {
            Self::Native => AssetId::native(CHAIN_NAME),
            Self::SplToken(mint) => AssetId {
                chain: CHAIN_NAME,
                asset: mint.to_string(),
            },
        }
    }
}
