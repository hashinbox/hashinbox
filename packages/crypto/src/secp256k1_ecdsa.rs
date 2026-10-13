use std::fmt;

use k256::ecdsa::SigningKey;
use zeroize::Zeroize as _;

use crate::{Error, ErrorKind, KEY_LENGTH, PrivateKey, SecretBytes, invalid_key};

/// A secp256k1 ECDSA key (Bitcoin P2WPKH, Ethereum). k256 zeroizes it on
/// drop; no `Clone` or Serde, and `Debug` is redacted.
pub struct Secp256k1Ecdsa(SigningKey);

impl PrivateKey for Secp256k1Ecdsa {
    /// A zero or out-of-range draw (probability ~2^-128) is an
    /// [`ErrorKind::InvalidKey`] error rather than a retry.
    fn generate() -> Result<Self, Error> {
        Self::from_bytes(crate::random_bytes()?.as_slice())
    }

    /// The length is checked here because k256 left-pads a short slice: part
    /// of a key would otherwise become a valid key for another address.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != KEY_LENGTH {
            return Err(invalid_key());
        }
        SigningKey::from_slice(bytes)
            .map(Self)
            .map_err(|_| invalid_key())
    }

    fn to_bytes(&self) -> SecretBytes {
        let mut bytes = self.0.to_bytes();
        let secret = SecretBytes::new(&bytes[..]);
        bytes[..].zeroize();
        secret
    }

    /// ECDSA over a 32-byte digest the caller hashed by its protocol's rules:
    /// 65 bytes, compact `r || s` then the recovery id.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let (signature, recovery_id) = self
            .0
            .sign_prehash_recoverable(&crate::digest(message)?)
            .map_err(|_| Error::new(ErrorKind::Signing, "ECDSA signing failed"))?;
        let mut bytes = signature.to_bytes().to_vec();
        bytes.push(recovery_id.to_byte());
        Ok(bytes)
    }
}

impl Secp256k1Ecdsa {
    /// 33-byte SEC1 compressed public key, `02/03 || x`.
    #[must_use]
    pub fn compressed_public_key(&self) -> [u8; 33] {
        let point = self.0.verifying_key().to_encoded_point(true);
        let mut bytes = [0; 33];
        bytes.copy_from_slice(point.as_bytes());
        bytes
    }

    /// 64-byte uncompressed public key without the SEC1 prefix, `x || y`.
    #[must_use]
    pub fn raw_public_key(&self) -> [u8; 64] {
        let point = self.0.verifying_key().to_encoded_point(false);
        let mut bytes = [0; 64];
        bytes.copy_from_slice(&point.as_bytes()[1..]);
        bytes
    }
}

impl fmt::Debug for Secp256k1Ecdsa {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secp256k1Ecdsa(..)")
    }
}
