//! Native Ethereum identities in their chain-neutral (canonical) forms.

use base::{AssetId, Networked, TransactionRef};

use crate::native::{Address, AssetKind, CHAIN_NAME, Error, ErrorKind, Network, TransactionId};

/// The chain-neutral identity of this network: `(ethereum, slug)`.
impl Networked for Network {
    fn canonical(&self) -> base::Network {
        base::Network::new(CHAIN_NAME, self.slug.clone())
    }
}

impl Address {
    /// This address inside `network`: lowercase `0x` hex, as indexing,
    /// wallets, and the API carry it. The address is the same on every EVM
    /// network.
    #[must_use]
    pub fn canonical(&self, network: &impl Networked) -> base::Address {
        base::Address::new(network.canonical(), self.to_string())
    }
}

/// Strict: an Ethereum network and exactly the canonical (lowercase) text.
/// Anything read back from storage, filters, or other wallets goes through
/// here; user input goes through `WalletFactory::parse_address`.
impl TryFrom<&base::Address> for Address {
    type Error = Error;

    fn try_from(address: &base::Address) -> Result<Self, Self::Error> {
        let invalid = |message: &str| Error::new(ErrorKind::InvalidAddress, message);
        if address.network.chain != CHAIN_NAME {
            return Err(invalid("address does not belong to Ethereum"));
        }
        let parsed = address.value.parse::<Self>()?;
        if parsed.to_string() != address.value {
            return Err(invalid("Ethereum address is not canonical lowercase text"));
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
    /// `ethereum/native`, or `ethereum/<lowercase contract address>`.
    #[must_use]
    pub fn id(&self) -> AssetId {
        match self {
            Self::Native => AssetId::native(CHAIN_NAME),
            Self::Erc20(token) => AssetId {
                chain: CHAIN_NAME,
                asset: token.to_string(),
            },
        }
    }
}
