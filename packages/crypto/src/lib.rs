//! Secret keys and signing, one type per scheme: `Secp256k1Ecdsa`,
//! `Secp256k1Schnorr` (BIP-340, `tweaked` for BIP-341) and `Ed25519`, all
//! behind `PrivateKey`. Keys zeroize on drop; `Debug` is redacted.

pub mod base58;
mod curve;
mod digest;
mod ed25519;
mod error;
pub mod mnemonic;
pub mod password;
mod random;
mod secp256k1_ecdsa;
mod secp256k1_schnorr;
mod secret;
pub mod totp;

pub use curve::Curve;
pub use digest::{crc32, sha256};
pub use ed25519::Ed25519;
pub use error::{Error, ErrorKind};
pub use random::random;
pub use secp256k1_ecdsa::Secp256k1Ecdsa;
pub use secp256k1_schnorr::Secp256k1Schnorr;
pub use secret::SecretBytes;

/// What every key does regardless of its scheme: create, import, export, sign.
pub trait PrivateKey: Sized {
    /// A new key from the operating-system random source.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Random`] when the operating system random source is
    /// unavailable.
    fn generate() -> Result<Self, Error>;

    /// Imports exactly 32 secret bytes.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidKey`] for another length or an invalid scalar.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Error>;

    /// The 32 secret bytes [`Self::from_bytes`] imports.
    fn to_bytes(&self) -> SecretBytes;

    /// The scheme's wire signature of `message`. Each type defines `message`:
    /// a 32-byte digest for secp256k1 (the caller hashes by its protocol's
    /// rules), the exact message for Ed25519.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Signing`] for a wrong digest length or a failed primitive.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error>;
}

/// Bytes in a secret key on either curve.
const KEY_LENGTH: usize = 32;

/// 32 bytes from the operating-system random source, zeroized on drop.
fn random_bytes() -> Result<zeroize::Zeroizing<[u8; KEY_LENGTH]>, Error> {
    let mut bytes = zeroize::Zeroizing::new([0_u8; KEY_LENGTH]);
    random::fill(bytes.as_mut_slice())?;
    Ok(bytes)
}

/// The 32-byte digest a secp256k1 scheme signs.
fn digest(message: &[u8]) -> Result<[u8; 32], Error> {
    message
        .try_into()
        .map_err(|_| Error::new(ErrorKind::Signing, "secp256k1 signs a 32-byte digest"))
}

fn invalid_key() -> Error {
    Error::new(ErrorKind::InvalidKey, "secret key must be 32 valid bytes")
}
