use crate::{
    PublicKey, PublicKeyFormat, SignRequest, Signature, SignedPayload, SignerError,
    secret_key::{SecretScalar, SecretSeed},
};

/// An in-process signing key on one curve. It names no address: a chain
/// derives its own address from the key.
///
/// The secret and every cryptographic operation on it live here; an SDK wraps
/// this key in whatever signing capability it offers. Every chain signs
/// through this type — Bitcoin and Ethereum on secp256k1, Solana on Ed25519 —
/// so the curve is the only thing that differs between them.
pub struct KeyPair {
    key: CurveKey,
}

/// The secret behind a [`KeyPair`]: one variant per curve, each answering
/// only the request shapes its curve defines.
enum CurveKey {
    Secp256k1(SecretScalar),
    Ed25519(SecretSeed),
}

impl KeyPair {
    /// Imports a 32-byte secp256k1 secret scalar; the bytes are zeroized on drop.
    ///
    /// # Errors
    ///
    /// [`crate::SignerErrorKind::InvalidRequest`] if the bytes are not a valid scalar.
    pub fn secp256k1(key: impl Into<Vec<u8>>) -> Result<Self, SignerError> {
        let key = SecretScalar::new(key).map_err(SignerError::from_crypto)?;
        Ok(Self {
            key: CurveKey::Secp256k1(key),
        })
    }

    /// Imports a 32-byte Ed25519 seed; the bytes are zeroized on drop.
    ///
    /// # Errors
    ///
    /// [`crate::SignerErrorKind::InvalidRequest`] if the bytes are not 32 long.
    pub fn ed25519(key: impl Into<Vec<u8>>) -> Result<Self, SignerError> {
        let key = SecretSeed::new(key).map_err(SignerError::from_crypto)?;
        Ok(Self {
            key: CurveKey::Ed25519(key),
        })
    }

    /// Derives this key's public key in `format`, so a chain that needs an
    /// address does not build a second copy of the secret.
    ///
    /// # Errors
    ///
    /// [`crate::SignerErrorKind::InvalidRequest`] if the curve rejects
    /// `format`, which only Ed25519 does: it has one raw 32-byte form, while
    /// every format exists on secp256k1.
    pub fn public_key(&self, format: PublicKeyFormat) -> Result<PublicKey, SignerError> {
        match &self.key {
            CurveKey::Secp256k1(key) => Ok(key.public_key(format)),
            CurveKey::Ed25519(key) => key.public_key(format).map_err(SignerError::from_crypto),
        }
    }

    /// Signs one request in this process, without a runtime.
    ///
    /// The name says *now* because a signing capability built on this key —
    /// `base::Signer`, which a remote signer also implements — offers an
    /// asynchronous `sign` on the very same type.
    ///
    /// What the key accepts follows its curve: secp256k1 signs a precomputed
    /// digest under ECDSA or BIP-340 Schnorr, Ed25519 signs the exact message,
    /// and only Schnorr accepts a key tweak.
    ///
    /// # Errors
    ///
    /// [`crate::SignerErrorKind::UnsupportedScheme`] for a scheme on another
    /// curve, [`crate::SignerErrorKind::UnsupportedOperation`] for a payload
    /// shape or key tweak this curve does not define, and
    /// [`crate::SignerErrorKind::InvalidRequest`] when a primitive rejects the
    /// digest, the encoding, or the tweak.
    pub fn sign_now(&self, request: SignRequest) -> Result<SignedPayload, SignerError> {
        let bytes = match &self.key {
            CurveKey::Secp256k1(key) => key.sign(&request)?,
            CurveKey::Ed25519(key) => key.sign(&request)?,
        };
        Ok(SignedPayload {
            signature: Signature {
                scheme: request.scheme,
                encoding: request.encoding,
                bytes,
            },
            public_key: self.public_key(request.public_key_format)?,
        })
    }
}
