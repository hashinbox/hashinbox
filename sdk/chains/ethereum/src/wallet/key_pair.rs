use alloy_primitives::keccak256;
use base::{Address as BaseAddress, Addresser, SignFuture, SignRequest, Signer};
use crypto::{PublicKeyFormat, SecretKey};
use wallets::{AddressEncoding, AddressFormat, AddressText, Error, ErrorKind, SecretBytes};

use crate::Address;

/// A local Ethereum key and its derived address, without RPC or indexing dependencies.
///
/// The address is independent of the EVM network and selected asset. Generation
/// keeps the secret in memory; it does not persist or expose it.
///
/// Secret-owning values cannot be printed through ordinary debugging:
/// ```compile_fail
/// let key = chain_ethereum::KeyPair::generate().unwrap();
/// println!("{key:?}");
/// ```
pub struct KeyPair {
    pub(super) pair: base::KeyPair<Address>,
}

impl KeyPair {
    /// Generates a key and derives its address locally using operating-system randomness.
    pub fn generate() -> Result<Self, Error> {
        let secret = SecretBytes::generate_secp256k1()
            .map_err(|error| Error::new(ErrorKind::Generation, error.to_string()))?;
        Self::import(secret)
    }

    /// Consumes an existing secret and derives the matching Ethereum address locally.
    pub fn import(secret: SecretBytes) -> Result<Self, Error> {
        let key = SecretKey::new(secret.as_bytes().to_vec())
            .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
        let public = key
            .public_key(PublicKeyFormat::Raw)
            .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
        let hash = keccak256(&public.bytes);
        let mut bytes = [0_u8; 20];
        bytes.copy_from_slice(&hash[12..]);
        let pair = base::KeyPair::new(Address(bytes), secret.as_bytes().to_vec())
            .map_err(|error| Error::new(ErrorKind::InvalidSecret, error.to_string()))?;
        Ok(Self { pair })
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
        let address = Address::try_from(address)
            .map_err(|error| Error::new(ErrorKind::InvalidAddress, error.to_string()))?;
        Ok(AddressText::new(AddressEncoding::Hex, address.to_string()))
    }

    fn parse_address(&self, address: &AddressText) -> Result<BaseAddress, Error> {
        if address.encoding != AddressEncoding::Hex {
            return Err(Error::new(
                ErrorKind::InvalidAddress,
                "Ethereum addresses use hexadecimal encoding",
            ));
        }
        address
            .text
            .parse::<Address>()
            .map(|parsed| parsed.address())
            .map_err(|error| Error::new(ErrorKind::InvalidAddress, error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use base::{Digest, SignablePayload, SignatureEncoding, SignatureScheme};
    use futures_executor::block_on;

    use super::*;

    #[test]
    fn local_import_derives_the_known_address_for_scalar_one() {
        let mut secret = [0; 32];
        secret[31] = 1;
        let key = KeyPair::import(SecretBytes::new(secret)).unwrap();
        let address = key.address();
        let text = key.address_text(&address).unwrap();
        assert_eq!(text.encoding, AddressEncoding::Hex);
        assert_eq!(text.text, "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf");
        assert_eq!(key.parse_address(&text).unwrap(), address);
    }

    #[test]
    fn local_import_rejects_invalid_secret_material() {
        for bytes in [Vec::new(), vec![1; 33], vec![0; 32], vec![255; 32]] {
            let error = KeyPair::import(SecretBytes::new(bytes)).err().unwrap();
            assert_eq!(error.kind, ErrorKind::InvalidSecret);
        }
    }

    #[test]
    fn generation_needs_no_network_or_asset_and_signs_for_its_address() {
        let key = KeyPair::generate().unwrap();
        let digest = [0x42; 32];
        let signed = block_on(key.sign(SignRequest {
            payload: SignablePayload::Digest(Digest {
                bytes: digest.to_vec(),
            }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }))
        .unwrap();
        let signature =
            alloy_primitives::Signature::try_from(signed.signature.bytes.as_slice()).unwrap();
        let recovered = signature
            .recover_address_from_prehash(&digest.into())
            .unwrap();
        assert_eq!(key.address().as_bytes(), recovered.as_slice());
        assert_eq!(signed.public_key.format, PublicKeyFormat::Raw);
        assert_eq!(signed.public_key.bytes.len(), 64);
    }

    #[test]
    fn local_address_format_rejects_wrong_width_encoding_and_text() {
        let key = KeyPair::import(SecretBytes::new([1; 32])).unwrap();
        for length in [0, 19, 21, 32] {
            let error = key
                .address_text(&BaseAddress::new(vec![0; length]))
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidAddress);
        }
        for address in [
            AddressText::new(AddressEncoding::Base58, "invalid"),
            AddressText::new(AddressEncoding::Hex, "invalid"),
        ] {
            assert_eq!(
                key.parse_address(&address).unwrap_err().kind,
                ErrorKind::InvalidAddress
            );
        }
    }
}
