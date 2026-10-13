use std::fmt;

use k256::{
    NonZeroScalar, Scalar,
    ecdsa::signature::hazmat::PrehashSigner as _,
    elliptic_curve::PrimeField as _,
    schnorr::{Signature, SigningKey},
};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroize as _;

use crate::{Error, ErrorKind, KEY_LENGTH, PrivateKey, SecretBytes, invalid_key};

/// A secp256k1 BIP-340 Schnorr key (Bitcoin Taproot). [`PrivateKey::from_bytes`]
/// is the untweaked key; [`Self::tweaked`] derives the BIP-341 output key.
/// k256 zeroizes it on drop; no `Clone` or Serde, and `Debug` is redacted.
pub struct Secp256k1Schnorr(SigningKey);

impl PrivateKey for Secp256k1Schnorr {
    /// A zero or out-of-range draw (probability ~2^-128) is an
    /// [`ErrorKind::InvalidKey`] error rather than a retry.
    fn generate() -> Result<Self, Error> {
        Self::from_bytes(crate::random_bytes()?.as_slice())
    }

    /// The length is checked because k256 accepts other lengths.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != KEY_LENGTH {
            return Err(invalid_key());
        }
        SigningKey::from_bytes(bytes)
            .map(Self)
            .map_err(|_| invalid_key())
    }

    /// The even-Y normalized secret: it imports to the same x-only key.
    fn to_bytes(&self) -> SecretBytes {
        let mut bytes = self.0.to_bytes();
        let secret = SecretBytes::new(&bytes[..]);
        bytes[..].zeroize();
        secret
    }

    /// BIP-340 Schnorr over a 32-byte digest: 64 bytes.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let signature: Signature = self
            .0
            .sign_prehash(&crate::digest(message)?)
            .map_err(|_| Error::new(ErrorKind::Signing, "Schnorr signing failed"))?;
        Ok(signature.to_bytes().to_vec())
    }
}

impl Secp256k1Schnorr {
    /// The BIP-341 tweaked key: the secret plus
    /// `TapTweak(x-only key || merkle_root)`; `None` is a key-path-only output.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidKey`] when the tweak is out of range or yields zero
    /// (probability ~2^-128).
    pub fn tweaked(&self, merkle_root: Option<[u8; 32]>) -> Result<Self, Error> {
        let invalid = || Error::new(ErrorKind::InvalidKey, "Taproot tweak is invalid");
        let tag = Sha256::digest(b"TapTweak");
        let mut hasher = Sha256::new();
        hasher.update(tag);
        hasher.update(tag);
        hasher.update(self.x_only_public_key());
        if let Some(root) = merkle_root {
            hasher.update(root);
        }
        let hash = hasher.finalize();
        let tweak = Option::<Scalar>::from(Scalar::from_repr(hash)).ok_or_else(invalid)?;
        let scalar = Option::<NonZeroScalar>::from(NonZeroScalar::new(
            *self.0.as_nonzero_scalar().as_ref() + tweak,
        ))
        .ok_or_else(invalid)?;
        Ok(Self(SigningKey::from(scalar)))
    }

    /// The 32-byte BIP-340 x-only public key.
    #[must_use]
    pub fn x_only_public_key(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes().into()
    }
}

impl fmt::Debug for Secp256k1Schnorr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secp256k1Schnorr(..)")
    }
}
