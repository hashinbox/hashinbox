use std::fmt;

use ed25519_dalek::{Signer as _, SigningKey};
use zeroize::Zeroizing;

use crate::{Error, KEY_LENGTH, PrivateKey, SecretBytes, invalid_key};

/// An Ed25519 seed (Solana). ed25519-dalek zeroizes it on drop; no `Clone`
/// or Serde, and `Debug` is redacted.
pub struct Ed25519(SigningKey);

impl PrivateKey for Ed25519 {
    fn generate() -> Result<Self, Error> {
        let seed = crate::random_bytes()?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let seed = Zeroizing::new(<[u8; KEY_LENGTH]>::try_from(bytes).map_err(|_| invalid_key())?);
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    fn to_bytes(&self) -> SecretBytes {
        SecretBytes::new(Zeroizing::new(self.0.to_bytes()).as_slice())
    }

    /// The 64-byte signature of the exact `message`; it never fails.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.0.sign(message).to_bytes().to_vec())
    }
}

impl Ed25519 {
    /// Imports a 64-byte keypair: the 32-byte secret, then its 32-byte public
    /// key, which must be the secret's own.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidKey`](crate::ErrorKind::InvalidKey) for another
    /// length, or a public half that is not the secret's.
    pub fn from_keypair(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 2 * KEY_LENGTH {
            return Err(invalid_key());
        }
        let (secret, public) = bytes.split_at(KEY_LENGTH);
        let key = Self::from_bytes(secret)?;
        if key.public_key() != public {
            return Err(Error::new(
                crate::ErrorKind::InvalidKey,
                "the keypair's public half is not its secret's",
            ));
        }
        Ok(key)
    }

    /// The 32-byte public key.
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }
}

impl fmt::Debug for Ed25519 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Ed25519(..)")
    }
}
