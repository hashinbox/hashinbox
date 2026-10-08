use std::str::FromStr;

use base::PublicKeyFormat;
use crypto::SecretBytes;
use zeroize::Zeroizing;

use super::{Address, Error, ErrorKind};

/// A local Ed25519 key and the address it controls. Solana addresses name no
/// cluster, so neither does the key.
///
/// The secret stays in memory; it is never persisted or exposed.
///
/// Secret-owning values cannot be printed through ordinary debugging:
/// ```compile_fail
/// let key = chain_solana::KeyPair::generate().unwrap();
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

    /// A new 32-byte Ed25519 seed from operating-system randomness, in the
    /// format [`Self::import`] consumes.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::KeyGeneration`] when no randomness is available.
    pub fn generate_secret() -> Result<SecretBytes, Error> {
        SecretBytes::generate_ed25519()
            .map_err(|error| Error::new(ErrorKind::KeyGeneration, error.to_string()))
    }

    /// Consumes an existing 32-byte Ed25519 seed and derives its address locally.
    ///
    /// The address is the seed's Ed25519 public key, so it is the one the
    /// signatures this key produces verify under.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidSecret`] unless `secret` has exactly 32 bytes.
    pub fn import(secret: SecretBytes) -> Result<Self, Error> {
        let invalid =
            |error: &dyn std::fmt::Display| Error::new(ErrorKind::InvalidSecret, error.to_string());
        let pair = crypto::KeyPair::ed25519(secret.as_bytes().to_vec())
            .map_err(|error| invalid(&error))?;
        let public = pair
            .public_key(PublicKeyFormat::Raw)
            .map_err(|error| invalid(&error))?;
        let mut bytes = [0_u8; solana_address::ADDRESS_BYTES];
        bytes.copy_from_slice(&public.bytes);
        Ok(Self {
            pair,
            address: Address::from_bytes(bytes),
        })
    }

    /// The address this key controls.
    #[must_use]
    pub const fn address(&self) -> &Address {
        &self.address
    }
}

/// The text form of an imported Solana secret: exactly 64 lowercase
/// hexadecimal characters. It converts into the [`SecretBytes`] that
/// [`KeyPair::import`] consumes.
///
/// This value intentionally implements neither `Clone`, `Debug`, `Display`,
/// nor Serde, so accepted secret material has no ordinary output path.
pub struct Seed(SecretBytes);

impl From<Seed> for SecretBytes {
    fn from(seed: Seed) -> Self {
        seed.0
    }
}

impl FromStr for Seed {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            Error::new(
                ErrorKind::InvalidSecret,
                "Solana secret must be exactly 64 lowercase hexadecimal characters",
            )
        };
        if input.len() != 64
            || !input
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid());
        }
        let mut bytes = Zeroizing::new([0_u8; 32]);
        hex::decode_to_slice(input, bytes.as_mut()).map_err(|_| invalid())?;
        Ok(Self(SecretBytes::new(*bytes)))
    }
}
