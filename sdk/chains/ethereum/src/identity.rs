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

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;

    #[test]
    fn canonical_network_is_the_chain_and_slug() {
        let network = Network::new(31_337, "anvil");
        assert_eq!(network.canonical(), base::Network::new(CHAIN_NAME, "anvil"));
    }

    #[test]
    fn sdk_ids_convert_exactly() {
        let id = TransactionId::from_bytes([0xab; 32]);
        assert_eq!(
            base::TransactionId::from(&id).as_str(),
            format!("0x{}", "ab".repeat(32))
        );
    }

    #[test]
    fn strict_conversion_rejects_foreign_chain_and_non_canonical_text() {
        let text = "0xaabbccddeeff0011223344556677889900aabbcc";
        for (network, value, message) in [
            (
                base::Network::new(ChainName::new("bitcoin"), "mainnet"),
                text.to_owned(),
                "address does not belong to Ethereum",
            ),
            (
                base::Network::new(CHAIN_NAME, "mainnet"),
                text.to_uppercase().replace("0X", "0x"),
                "Ethereum address is not canonical lowercase text",
            ),
        ] {
            let error = Address::try_from(&base::Address::new(network, value)).unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidAddress);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn canonical_identities_preserve_network_leading_zeroes_and_lowercase_hex() {
        let network = base::Network::new(CHAIN_NAME, "test");
        let mut bytes = [0; 20];
        bytes[18..].copy_from_slice(&[0xab, 0xff]);
        let canonical = Address::from_bytes(bytes).canonical(&network);
        assert_eq!(canonical.network, network);
        assert_eq!(canonical.value, format!("0x{}abff", "00".repeat(18)));
        assert_eq!(
            Address::try_from(&canonical),
            Ok(Address::from_bytes(bytes))
        );

        let id = TransactionId::from_bytes(std::array::from_fn(|index| index as u8));
        let reference = id.canonical(&network);
        assert!(reference.belongs_to(&network));
        assert_eq!(
            reference.id.as_str(),
            "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        );
        assert_eq!(AssetKind::Native.id(), AssetId::native(CHAIN_NAME));
        assert_eq!(
            AssetKind::Erc20(Address::from_bytes([0xab; 20])).id().asset,
            format!("0x{}", "ab".repeat(20))
        );
    }
}
