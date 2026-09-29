use base::PublicKeyFormat;
use bitcoin::{
    Address as NativeAddress, CompressedPublicKey, PublicKey, XOnlyPublicKey, secp256k1::Secp256k1,
};
use crypto::SecretBytes;

use super::{Address, AddressType, Error, ErrorKind, Network};

/// A local secp256k1 key. It names no network: the wallet it is attached to
/// chooses the network and [`AddressType`], and [`KeyPair::address`] derives
/// the address from them.
///
/// The secret stays in memory; it is never persisted or exposed.
///
/// Secret-owning values cannot be printed through ordinary debugging:
/// ```compile_fail
/// let key = chain_bitcoin::KeyPair::generate().unwrap();
/// println!("{key:?}");
/// ```
pub struct KeyPair {
    /// The signing key; it signs digests for any address it derives.
    pub(crate) pair: crypto::KeyPair,
    compressed: CompressedPublicKey,
    x_only: XOnlyPublicKey,
}

impl KeyPair {
    /// Generates a key from operating-system randomness.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::KeyGeneration`] when no randomness is available.
    pub fn generate() -> Result<Self, Error> {
        Self::import(Self::generate_secret()?)
    }

    /// A new 32-byte secp256k1 secret from operating-system randomness, in
    /// the format [`Self::import`] consumes.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::KeyGeneration`] when no randomness is available.
    pub fn generate_secret() -> Result<SecretBytes, Error> {
        SecretBytes::generate_secp256k1()
            .map_err(|error| Error::new(ErrorKind::KeyGeneration, error.to_string()))
    }

    /// Consumes an existing 32-byte secret and derives its public keys locally.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidSecret`] when `secret` is not a valid scalar.
    pub fn import(secret: SecretBytes) -> Result<Self, Error> {
        let invalid =
            |error: &dyn std::fmt::Display| Error::new(ErrorKind::InvalidSecret, error.to_string());
        let pair = crypto::KeyPair::secp256k1(secret.as_bytes().to_vec())
            .map_err(|error| invalid(&error))?;
        let compressed = pair
            .public_key(PublicKeyFormat::Compressed)
            .map_err(|error| invalid(&error))?;
        let compressed = PublicKey::from_slice(&compressed.bytes)
            .map_err(|error| invalid(&error))
            .and_then(|key| CompressedPublicKey::try_from(key).map_err(|error| invalid(&error)))?;
        let x_only = XOnlyPublicKey::from(compressed.0);
        Ok(Self {
            pair,
            compressed,
            x_only,
        })
    }

    /// The address of `kind` this key controls on `network`.
    #[must_use]
    pub fn address(&self, network: Network, kind: AddressType) -> Address {
        let native = match kind {
            AddressType::SegwitV0 => NativeAddress::p2wpkh(&self.compressed, network.native()),
            AddressType::Taproot => NativeAddress::p2tr(
                &Secp256k1::verification_only(),
                self.x_only,
                None,
                network.native(),
            ),
        };
        Address::unchecked(native.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_key_derives_each_address_type_on_each_network() {
        let key = KeyPair::import(SecretBytes::new([1; 32])).unwrap();
        for (kind, check) in [
            (AddressType::SegwitV0, bitcoin::AddressType::P2wpkh),
            (AddressType::Taproot, bitcoin::AddressType::P2tr),
        ] {
            let regtest = key.address(Network::Regtest, kind);
            let parsed = Address::parse(regtest.as_str(), Network::Regtest).unwrap();
            assert_eq!(parsed, regtest);
            let native = regtest
                .as_str()
                .parse::<NativeAddress<_>>()
                .unwrap()
                .assume_checked();
            assert_eq!(native.address_type(), Some(check));
            assert_ne!(key.address(Network::Mainnet, kind), regtest);
        }
    }

    #[test]
    fn local_import_rejects_invalid_secret_material() {
        for bytes in [Vec::new(), vec![1; 33], vec![0; 32], vec![255; 32]] {
            let error = KeyPair::import(SecretBytes::new(bytes)).err().unwrap();
            assert_eq!(error.kind, ErrorKind::InvalidSecret);
        }
    }
}
