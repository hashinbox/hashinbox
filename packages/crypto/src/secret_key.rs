use ed25519_dalek::{Signer as _, SigningKey};
use k256::{
    NonZeroScalar, Scalar, ecdsa::SigningKey as EcdsaSigningKey,
    ecdsa::signature::hazmat::PrehashSigner, elliptic_curve::PrimeField,
    schnorr::SigningKey as SchnorrSigningKey,
};
use zeroize::Zeroizing;

use crate::{
    Curve, Error, ErrorKind, PublicKey, PublicKeyFormat, ScalarTweak, SignRequest, SignablePayload,
    SignatureEncoding, SignatureScheme, SignerError, SignerErrorKind,
};

/// An in-memory secp256k1 secret scalar, the engine behind a secp256k1
/// [`crate::KeyPair`].
///
/// The scalar lives inside `k256::ecdsa::SigningKey`, which zeroizes it on
/// drop. It is built once, in [`Self::new`], so no later operation can find it
/// invalid; like its Ed25519 sibling this value implements neither `Clone`,
/// `Debug`, nor Serde.
pub(crate) struct SecretScalar {
    key: EcdsaSigningKey,
}

impl SecretScalar {
    /// Bytes in a secp256k1 secret scalar.
    const LENGTH: usize = 32;

    /// Validates 32 bytes as a secp256k1 scalar ([`ErrorKind::InvalidKey`] otherwise).
    ///
    /// The length is checked here rather than left to the curve, which
    /// left-pads a short slice: 31 bytes of a caller's key would otherwise
    /// become a valid scalar controlling an address their real key does not.
    /// This is the length check for every secp256k1 chain, so no wallet
    /// factory needs one of its own.
    ///
    /// This is also the only place the scalar can be rejected: the key built
    /// here is the one every operation uses, so none of them re-derives it and
    /// none of them has an invalid-key branch.
    pub(crate) fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, Error> {
        let bytes = Zeroizing::new(bytes.into());
        let invalid = || {
            Error::new(
                ErrorKind::InvalidKey,
                "secret key must be a valid 32-byte secp256k1 scalar",
            )
        };
        if bytes.len() != Self::LENGTH {
            return Err(invalid());
        }
        Ok(Self {
            key: EcdsaSigningKey::from_slice(&bytes).map_err(|_| invalid())?,
        })
    }

    /// Derives the public key in `format`; every format exists on this curve,
    /// so this cannot fail.
    pub(crate) fn public_key(&self, format: PublicKeyFormat) -> PublicKey {
        let verifying = self.key.verifying_key();
        let encoded = verifying.to_encoded_point(false);
        let raw = &encoded.as_bytes()[1..];
        let bytes = match format {
            PublicKeyFormat::Raw => raw.to_vec(),
            PublicKeyFormat::XOnly => raw[..32].to_vec(),
            PublicKeyFormat::Uncompressed => encoded.as_bytes().to_vec(),
            PublicKeyFormat::Compressed => verifying.to_encoded_point(true).as_bytes().to_vec(),
        };
        PublicKey {
            curve: Curve::Secp256k1,
            format,
            bytes,
        }
    }

    /// Answers `request`: a precomputed digest under ECDSA or BIP-340 Schnorr.
    ///
    /// A raw message is refused because secp256k1 signs what the protocol
    /// already hashed, and an Ed25519 request is refused because this key is
    /// on the wrong curve for it.
    pub(crate) fn sign(&self, request: &SignRequest) -> Result<Vec<u8>, SignerError> {
        let SignablePayload::Digest(digest) = &request.payload else {
            return Err(SignerError::new(
                SignerErrorKind::UnsupportedOperation,
                "signer accepts precomputed digests only",
            ));
        };
        let signature = match request.scheme {
            SignatureScheme::EcdsaSecp256k1 => {
                if request.key_tweak.is_some() {
                    return Err(SignerError::new(
                        SignerErrorKind::UnsupportedOperation,
                        "ECDSA does not accept a key tweak",
                    ));
                }
                self.sign_ecdsa(&digest.bytes, request.encoding)
            }
            SignatureScheme::SchnorrSecp256k1 => {
                self.sign_schnorr(&digest.bytes, request.encoding, request.key_tweak.as_ref())
            }
            SignatureScheme::Ed25519 => {
                return Err(SignerError::new(
                    SignerErrorKind::UnsupportedScheme,
                    "a secp256k1 key signs no Ed25519 request",
                ));
            }
        };
        signature.map_err(SignerError::from_crypto)
    }

    /// Signs a 32-byte digest with ECDSA; the caller hashes by its protocol's rules.
    fn sign_ecdsa(&self, digest: &[u8], encoding: SignatureEncoding) -> Result<Vec<u8>, Error> {
        let digest: [u8; 32] = digest.try_into().map_err(|_| {
            Error::new(
                ErrorKind::InvalidDigest,
                "digest must contain exactly 32 bytes",
            )
        })?;
        let (signature, recovery_id) = self
            .key
            .sign_prehash_recoverable(&digest)
            .map_err(|_| Error::new(ErrorKind::Signing, "ECDSA signing failed"))?;
        match encoding {
            SignatureEncoding::Recoverable => {
                let mut bytes = signature.to_bytes().to_vec();
                bytes.push(recovery_id.to_byte());
                Ok(bytes)
            }
            SignatureEncoding::Compact | SignatureEncoding::Raw => {
                Ok(signature.to_bytes().to_vec())
            }
            SignatureEncoding::Der => Ok(signature.to_der().as_bytes().to_vec()),
        }
    }

    /// Signs a 32-byte digest with BIP-340 Schnorr, after applying `tweak`.
    ///
    /// Only [`SignatureEncoding::Raw`] (64 bytes) is supported.
    fn sign_schnorr(
        &self,
        digest: &[u8],
        encoding: SignatureEncoding,
        tweak: Option<&ScalarTweak>,
    ) -> Result<Vec<u8>, Error> {
        if encoding != SignatureEncoding::Raw {
            return Err(Error::new(
                ErrorKind::UnsupportedEncoding,
                "Schnorr signatures require raw encoding",
            ));
        }
        let digest: [u8; 32] = digest.try_into().map_err(|_| {
            Error::new(
                ErrorKind::InvalidDigest,
                "digest must contain exactly 32 bytes",
            )
        })?;
        // BIP-340 signs with the same scalar, so it is converted rather than
        // re-parsed: `new` already proved the scalar is on the curve.
        let mut key = SchnorrSigningKey::from(*self.key.as_nonzero_scalar());
        if let Some(tweak) = tweak {
            key = Self::tweaked(&key, tweak)?;
        }
        let signature: k256::schnorr::Signature = PrehashSigner::sign_prehash(&key, &digest)
            .map_err(|_| Error::new(ErrorKind::Signing, "Schnorr signing failed"))?;
        Ok(signature.to_bytes().to_vec())
    }

    /// `key` with `tweak` added to its scalar.
    fn tweaked(key: &SchnorrSigningKey, tweak: &ScalarTweak) -> Result<SchnorrSigningKey, Error> {
        let bytes = tweak.scalar(&key.verifying_key().to_bytes())?;
        let tweak = Option::<Scalar>::from(Scalar::from_repr(bytes.into()))
            .ok_or_else(|| Error::new(ErrorKind::InvalidTweak, "invalid scalar tweak"))?;
        let scalar = *key.as_nonzero_scalar().as_ref() + tweak;
        let scalar =
            Option::<NonZeroScalar>::from(NonZeroScalar::new(scalar)).ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidTweak,
                    "scalar tweak produced the zero key",
                )
            })?;
        Ok(SchnorrSigningKey::from(scalar))
    }
}

/// An in-memory Ed25519 secret seed, the engine behind an Ed25519
/// [`crate::KeyPair`].
///
/// The expanded secret lives inside `ed25519_dalek::SigningKey`, which
/// zeroizes it on drop; like its secp256k1 sibling this value implements
/// neither `Clone`, `Debug`, nor Serde.
pub(crate) struct SecretSeed {
    key: SigningKey,
}

impl SecretSeed {
    /// Bytes in an Ed25519 seed.
    const LENGTH: usize = 32;

    /// Validates 32 bytes as an Ed25519 seed ([`ErrorKind::InvalidKey`] otherwise).
    ///
    /// Only the length is checked, and it is checked here for the same reason
    /// the secp256k1 length is: the scheme hashes the seed into a scalar, so a
    /// short or long slice would either be padded or silently truncated into a
    /// key controlling an address the caller does not own. Every 32-byte
    /// string is a valid seed, so nothing else can be rejected.
    pub(crate) fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, Error> {
        let bytes = Zeroizing::new(bytes.into());
        let seed = Zeroizing::new(<[u8; Self::LENGTH]>::try_from(bytes.as_slice()).map_err(
            |_| {
                Error::new(
                    ErrorKind::InvalidKey,
                    "secret key must be exactly 32 Ed25519 seed bytes",
                )
            },
        )?);
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Derives the public key in `format`; Ed25519 has one 32-byte form.
    pub(crate) fn public_key(&self, format: PublicKeyFormat) -> Result<PublicKey, Error> {
        if format != PublicKeyFormat::Raw {
            return Err(Error::new(
                ErrorKind::UnsupportedEncoding,
                "Ed25519 public keys have one raw 32-byte form",
            ));
        }
        Ok(PublicKey {
            curve: Curve::Ed25519,
            format,
            bytes: self.key.verifying_key().to_bytes().to_vec(),
        })
    }

    /// Answers `request`: the exact message, signed with Ed25519.
    ///
    /// A precomputed digest is refused because Ed25519 hashes the message
    /// itself, a secp256k1 scheme because this key is on the wrong curve, and
    /// a key tweak because only BIP-340 Schnorr defines one.
    pub(crate) fn sign(&self, request: &SignRequest) -> Result<Vec<u8>, SignerError> {
        let SignablePayload::Message(message) = &request.payload else {
            return Err(SignerError::new(
                SignerErrorKind::UnsupportedOperation,
                "signer accepts whole messages only",
            ));
        };
        if request.scheme != SignatureScheme::Ed25519 {
            return Err(SignerError::new(
                SignerErrorKind::UnsupportedScheme,
                "an Ed25519 key signs no secp256k1 request",
            ));
        }
        if request.key_tweak.is_some() {
            return Err(SignerError::new(
                SignerErrorKind::UnsupportedOperation,
                "Ed25519 does not accept a key tweak",
            ));
        }
        self.sign_message(message, request.encoding)
            .map_err(SignerError::from_crypto)
    }

    /// Signs `message` exactly; Ed25519 has one 64-byte signature encoding.
    fn sign_message(&self, message: &[u8], encoding: SignatureEncoding) -> Result<Vec<u8>, Error> {
        if encoding != SignatureEncoding::Raw {
            return Err(Error::new(
                ErrorKind::UnsupportedEncoding,
                "Ed25519 signatures require raw encoding",
            ));
        }
        Ok(self.key.sign(message).to_bytes().to_vec())
    }
}
