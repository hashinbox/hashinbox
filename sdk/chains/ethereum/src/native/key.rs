use alloy_primitives::keccak256;
use base::PublicKeyFormat;
use crypto::SecretBytes;

use super::{Address, Error, ErrorKind};

/// A local secp256k1 key and its derived Ethereum address.
///
/// The address is independent of the EVM network and the asset. The secret
/// stays in memory; it is never persisted or exposed.
///
/// Secret-owning values cannot be printed through ordinary debugging:
/// ```compile_fail
/// let key = chain_ethereum::KeyPair::generate().unwrap();
/// println!("{key:?}");
/// ```
pub struct KeyPair {
    pub(crate) pair: crypto::KeyPair,
    pub(crate) address: Address,
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

    /// Consumes an existing 32-byte secret and derives its address locally.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidSecret`] when `secret` is not a valid scalar.
    pub fn import(secret: SecretBytes) -> Result<Self, Error> {
        let invalid =
            |error: &dyn std::fmt::Display| Error::new(ErrorKind::InvalidSecret, error.to_string());
        let pair = crypto::KeyPair::secp256k1(secret.as_bytes().to_vec())
            .map_err(|error| invalid(&error))?;
        let public = pair
            .public_key(PublicKeyFormat::Raw)
            .map_err(|error| invalid(&error))?;
        let hash = keccak256(&public.bytes);
        let mut bytes = [0_u8; 20];
        bytes.copy_from_slice(&hash[12..]);
        Ok(Self {
            pair,
            address: Address::from_bytes(bytes),
        })
    }

    /// The account this key signs for.
    #[must_use]
    pub const fn address(&self) -> &Address {
        &self.address
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_import_derives_the_known_address_for_scalar_one() {
        let mut secret = [0; 32];
        secret[31] = 1;
        let key = KeyPair::import(SecretBytes::new(secret)).unwrap();
        assert_eq!(
            key.address().to_string(),
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }

    #[test]
    fn local_import_rejects_invalid_secret_material() {
        for bytes in [Vec::new(), vec![1; 33], vec![0; 32], vec![255; 32]] {
            let error = KeyPair::import(SecretBytes::new(bytes)).err().unwrap();
            assert_eq!(error.kind, ErrorKind::InvalidSecret);
        }
    }
}
