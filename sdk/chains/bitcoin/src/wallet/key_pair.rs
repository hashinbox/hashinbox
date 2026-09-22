use base::{Address as BaseAddress, Addresser, SignFuture, SignRequest, Signer};
use bitcoin::{
    Address as NativeAddress, CompressedPublicKey, PublicKey, XOnlyPublicKey, secp256k1::Secp256k1,
};
use crypto::{PublicKeyFormat, SecretKey};
use wallets::{AddressEncoding, AddressFormat, AddressText, Error, ErrorKind, SecretBytes};

use crate::{Address, Network};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressType {
    SegwitV0,
    Taproot,
}

/// A local Bitcoin key and its derived address, without RPC or indexing dependencies.
///
/// Implements address formatting and signing, but not balance, history, or sending.
/// Generation keeps the secret in memory; it does not persist or expose it.
///
/// Secret-owning values cannot be printed through ordinary debugging:
/// ```compile_fail
/// use chain_bitcoin::{AddressType, KeyPair, Network};
/// let key = KeyPair::generate(Network::Regtest, AddressType::SegwitV0).unwrap();
/// println!("{key:?}");
/// ```
pub struct KeyPair {
    pub(super) network: Network,
    pub(super) address_type: AddressType,
    pub(super) pair: base::KeyPair<Address>,
}

impl KeyPair {
    /// Generates a key and derives its address locally using operating-system randomness.
    pub fn generate(network: Network, address_type: AddressType) -> Result<Self, Error> {
        let secret = SecretBytes::generate_secp256k1()
            .map_err(|error| Error::new(ErrorKind::Generation, error.to_string()))?;
        Self::import(secret, network, address_type)
    }

    /// Consumes an existing secret and derives the matching Bitcoin address locally.
    pub fn import(
        secret: SecretBytes,
        network: Network,
        address_type: AddressType,
    ) -> Result<Self, Error> {
        let key = SecretKey::new(secret.as_bytes().to_vec())
            .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
        let format = match address_type {
            AddressType::SegwitV0 => PublicKeyFormat::Compressed,
            AddressType::Taproot => PublicKeyFormat::XOnly,
        };
        let public = key
            .public_key(format)
            .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
        let address = address_type.address(&public, network)?;
        let pair = base::KeyPair::new(address, secret.as_bytes().to_vec())
            .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
        Ok(Self {
            network,
            address_type,
            pair,
        })
    }
}

impl AddressType {
    fn address(self, public: &crypto::PublicKey, network: Network) -> Result<Address, Error> {
        let native = match self {
            Self::SegwitV0 => {
                let key = PublicKey::from_slice(&public.bytes)
                    .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
                let key = CompressedPublicKey::try_from(key)
                    .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
                NativeAddress::p2wpkh(&key, network.native())
            }
            Self::Taproot => {
                let key = XOnlyPublicKey::from_slice(&public.bytes)
                    .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
                NativeAddress::p2tr(&Secp256k1::verification_only(), key, None, network.native())
            }
        };
        Ok(Address::from_encoded(native.to_string()))
    }
}

impl Addresser for KeyPair {
    fn address(&self) -> BaseAddress {
        self.pair.address()
    }
}

impl Signer for KeyPair {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        self.pair.sign(request)
    }
}

impl AddressFormat for KeyPair {
    fn address_text(&self, address: &BaseAddress) -> Result<AddressText, Error> {
        let text = std::str::from_utf8(address.as_bytes()).map_err(|_| {
            Error::new(
                ErrorKind::InvalidAddress,
                "Bitcoin address bytes are not UTF-8",
            )
        })?;
        let parsed = text
            .parse::<NativeAddress<bitcoin::address::NetworkUnchecked>>()
            .map_err(|error| Error::new(ErrorKind::InvalidAddress, error.to_string()))?
            .require_network(self.network.native())
            .map_err(|error| Error::new(ErrorKind::InvalidAddress, error.to_string()))?;
        let encoding = match parsed.address_type() {
            Some(bitcoin::AddressType::P2pkh | bitcoin::AddressType::P2sh) => {
                AddressEncoding::Base58Check
            }
            Some(bitcoin::AddressType::P2tr | bitcoin::AddressType::P2a) => {
                AddressEncoding::Bech32m
            }
            Some(bitcoin::AddressType::P2wpkh | bitcoin::AddressType::P2wsh) => {
                AddressEncoding::Bech32
            }
            None | Some(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidAddress,
                    "Bitcoin address type is unsupported",
                ));
            }
        };
        Ok(AddressText::new(encoding, parsed.to_string()))
    }

    fn parse_address(&self, address: &AddressText) -> Result<BaseAddress, Error> {
        let parsed = Address::parse_for_network(&address.text, self.network)
            .map_err(|error| Error::new(ErrorKind::InvalidAddress, error.to_string()))?;
        let canonical = self.address_text(&parsed.address())?;
        if canonical.encoding != address.encoding {
            return Err(Error::new(
                ErrorKind::InvalidAddress,
                "Bitcoin address encoding does not match its text",
            ));
        }
        Ok(parsed.address())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_needs_only_network_and_address_type() {
        for (address_type, encoding, native_type) in [
            (
                AddressType::SegwitV0,
                AddressEncoding::Bech32,
                bitcoin::AddressType::P2wpkh,
            ),
            (
                AddressType::Taproot,
                AddressEncoding::Bech32m,
                bitcoin::AddressType::P2tr,
            ),
        ] {
            let key = KeyPair::generate(Network::Regtest, address_type).unwrap();
            let text = key.address_text(&key.address()).unwrap();
            let parsed = text
                .text
                .parse::<NativeAddress<_>>()
                .unwrap()
                .require_network(bitcoin::Network::Regtest)
                .unwrap();
            assert_eq!(text.encoding, encoding);
            assert_eq!(parsed.address_type(), Some(native_type));
            assert_eq!(key.parse_address(&text).unwrap(), key.address());
        }
    }

    #[test]
    fn local_import_rejects_invalid_secret_material() {
        for bytes in [Vec::new(), vec![1; 33], vec![0; 32], vec![255; 32]] {
            let error = KeyPair::import(
                SecretBytes::new(bytes),
                Network::Regtest,
                AddressType::SegwitV0,
            )
            .err()
            .unwrap();
            assert_eq!(error.kind, ErrorKind::InvalidSecret);
        }
    }

    #[test]
    fn destination_format_is_independent_of_own_address_type() {
        let source = KeyPair::generate(Network::Regtest, AddressType::SegwitV0).unwrap();
        let target = KeyPair::generate(Network::Regtest, AddressType::Taproot).unwrap();
        let mut text = target.address_text(&target.address()).unwrap();
        assert_eq!(source.parse_address(&text).unwrap(), target.address());
        text.encoding = AddressEncoding::Bech32;
        assert_eq!(
            source.parse_address(&text).unwrap_err().kind,
            ErrorKind::InvalidAddress
        );

        let mainnet = KeyPair::generate(Network::Mainnet, AddressType::SegwitV0).unwrap();
        let text = mainnet.address_text(&mainnet.address()).unwrap();
        assert_eq!(
            source.parse_address(&text).unwrap_err().kind,
            ErrorKind::InvalidAddress
        );
    }
}
