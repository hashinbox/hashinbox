//! Native Bitcoin identities in their chain-neutral (canonical) forms.

use base::{Networked, TransactionRef};
use indexing::{AssetId, OutputId};

use crate::native::{
    Address, AssetKind, CHAIN_NAME, Error, ErrorKind, Network, TransactionId, block::Outpoint,
};

/// The chain-neutral identity of this network: `(bitcoin, slug)`.
impl Networked for Network {
    fn canonical(&self) -> base::Network {
        base::Network::new(CHAIN_NAME, self.slug())
    }
}

impl Address {
    /// This address inside `network`: its canonical text, as indexing,
    /// wallets, and the API carry it.
    #[must_use]
    pub fn canonical(&self, network: &impl Networked) -> base::Address {
        base::Address::new(network.canonical(), self.to_string())
    }
}

/// Strict: a Bitcoin network, a known network, and exactly the canonical text
/// of an address of that network. Anything read back from storage, filters,
/// or other wallets goes through here; user input goes through
/// `WalletFactory::parse_address`.
impl TryFrom<&base::Address> for Address {
    type Error = Error;

    fn try_from(address: &base::Address) -> Result<Self, Self::Error> {
        let invalid = |message: &str| Error::new(ErrorKind::InvalidAddress, message);
        if address.network.chain != CHAIN_NAME {
            return Err(invalid("address does not belong to Bitcoin"));
        }
        let network = Network::from_slug(&address.network.name)
            .ok_or_else(|| invalid("address names an unknown Bitcoin network"))?;
        let parsed = Self::parse(&address.value, network)?;
        if parsed.as_str() != address.value {
            return Err(invalid("Bitcoin address is not canonical text"));
        }
        Ok(parsed)
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
    /// The asset's identity in the index.
    #[must_use]
    pub fn id(self) -> AssetId {
        match self {
            Self::Native => AssetId::native(CHAIN_NAME),
        }
    }
}

impl Outpoint {
    /// This output inside `network`, as the output index stores it.
    #[must_use]
    pub fn canonical(&self, network: &impl Networked) -> OutputId {
        OutputId {
            transaction: self.transaction_id.canonical(network),
            index: self.output_index,
        }
    }
}
