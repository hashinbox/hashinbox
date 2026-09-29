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

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn digest(bytes: Vec<u8>) -> SignablePayload {
        SignablePayload::Digest(crate::Digest { bytes })
    }

    fn request(payload: SignablePayload, scheme: SignatureScheme) -> SignRequest {
        SignRequest {
            payload,
            scheme,
            encoding: SignatureEncoding::Raw,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }
    }

    #[test]
    fn signing_rejects_every_non_32_byte_digest_at_the_boundary() {
        let key = SecretScalar::new([1_u8; 32]).expect("test key must be valid");
        let expected = Error::new(
            ErrorKind::InvalidDigest,
            "digest must contain exactly 32 bytes",
        );
        for length in [0, 1, 31, 33, 64] {
            let bytes = vec![2; length];
            assert_eq!(
                key.sign_ecdsa(&bytes, SignatureEncoding::Recoverable),
                Err(expected.clone())
            );
            assert_eq!(
                key.sign_schnorr(&bytes, SignatureEncoding::Raw, None),
                Err(expected.clone())
            );
        }
    }

    #[test]
    fn schnorr_validates_encoding_then_digest_then_tweak() {
        let key = SecretScalar::new([1_u8; 32]).expect("test key must be valid");
        let tweak = ScalarTweak::Add([255; 32]);
        for encoding in [
            SignatureEncoding::Der,
            SignatureEncoding::Compact,
            SignatureEncoding::Recoverable,
        ] {
            assert_eq!(
                key.sign_schnorr(&[], encoding, Some(&tweak)),
                Err(Error::new(
                    ErrorKind::UnsupportedEncoding,
                    "Schnorr signatures require raw encoding",
                ))
            );
        }
        assert_eq!(
            key.sign_schnorr(&[], SignatureEncoding::Raw, Some(&tweak)),
            Err(Error::new(
                ErrorKind::InvalidDigest,
                "digest must contain exactly 32 bytes",
            ))
        );
        assert_eq!(
            key.sign_schnorr(&[2; 32], SignatureEncoding::Raw, Some(&tweak)),
            Err(Error::new(ErrorKind::InvalidTweak, "invalid scalar tweak"))
        );
    }

    #[test]
    fn signs_ecdsa_and_schnorr_digests() {
        let key = SecretScalar::new([1_u8; 32]).expect("test key must be valid");
        assert_eq!(
            key.sign_ecdsa(&[2; 32], SignatureEncoding::Recoverable)
                .expect("ECDSA must sign")
                .len(),
            65
        );
        assert_eq!(
            key.sign_schnorr(&[3; 32], SignatureEncoding::Raw, None)
                .expect("Schnorr must sign")
                .len(),
            64
        );
    }

    #[test]
    fn an_ed25519_seed_reproduces_the_rfc_8032_test_vector() {
        let seed = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        let key = SecretSeed::new(seed).expect("a 32-byte seed is always valid");
        let public = key
            .public_key(PublicKeyFormat::Raw)
            .expect("Ed25519 has a raw public key");

        assert_eq!(public.curve, Curve::Ed25519);
        assert_eq!(
            hex(&public.bytes),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert_eq!(
            hex(&key
                .sign(&request(
                    SignablePayload::Message(Vec::new()),
                    SignatureScheme::Ed25519
                ))
                .expect("the empty message signs")),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33\
             bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn an_ed25519_key_refuses_every_length_but_32_and_every_format_but_raw() {
        for bytes in [Vec::new(), vec![3; 31], vec![3; 33], vec![3; 64]] {
            assert_eq!(
                SecretSeed::new(bytes).err().map(|error| error.kind),
                Some(ErrorKind::InvalidKey)
            );
        }
        let key = SecretSeed::new([3_u8; 32]).expect("a 32-byte seed is always valid");
        for format in [
            PublicKeyFormat::Compressed,
            PublicKeyFormat::Uncompressed,
            PublicKeyFormat::XOnly,
        ] {
            assert_eq!(
                key.public_key(format),
                Err(Error::new(
                    ErrorKind::UnsupportedEncoding,
                    "Ed25519 public keys have one raw 32-byte form",
                ))
            );
        }
    }

    #[test]
    fn each_curve_refuses_the_other_curves_request_shape() {
        let secp = SecretScalar::new([1_u8; 32]).expect("test key must be valid");
        let ed = SecretSeed::new([1_u8; 32]).expect("a 32-byte seed is always valid");
        let message = || SignablePayload::Message(vec![9; 32]);

        for (error, kind, message) in [
            (
                secp.sign(&request(message(), SignatureScheme::EcdsaSecp256k1)),
                SignerErrorKind::UnsupportedOperation,
                "signer accepts precomputed digests only",
            ),
            (
                secp.sign(&request(digest(vec![9; 32]), SignatureScheme::Ed25519)),
                SignerErrorKind::UnsupportedScheme,
                "a secp256k1 key signs no Ed25519 request",
            ),
            (
                ed.sign(&request(digest(vec![9; 32]), SignatureScheme::Ed25519)),
                SignerErrorKind::UnsupportedOperation,
                "signer accepts whole messages only",
            ),
            (
                ed.sign(&request(message(), SignatureScheme::EcdsaSecp256k1)),
                SignerErrorKind::UnsupportedScheme,
                "an Ed25519 key signs no secp256k1 request",
            ),
        ] {
            let error = error.expect_err("the request must be refused");
            assert_eq!((error.kind, error.message.as_str()), (kind, message));
        }
    }

    #[test]
    fn an_ed25519_key_refuses_a_tweak_and_every_encoding_but_raw() {
        let key = SecretSeed::new([1_u8; 32]).expect("a 32-byte seed is always valid");
        let mut tweaked = request(SignablePayload::Message(vec![]), SignatureScheme::Ed25519);
        tweaked.key_tweak = Some(ScalarTweak::Add([1; 32]));
        let error = key.sign(&tweaked).expect_err("Ed25519 has no key tweak");
        assert_eq!(
            (error.kind, error.message.as_str()),
            (
                SignerErrorKind::UnsupportedOperation,
                "Ed25519 does not accept a key tweak"
            )
        );
        for encoding in [
            SignatureEncoding::Der,
            SignatureEncoding::Compact,
            SignatureEncoding::Recoverable,
        ] {
            let mut encoded = request(SignablePayload::Message(vec![]), SignatureScheme::Ed25519);
            encoded.encoding = encoding;
            let error = key.sign(&encoded).expect_err("Ed25519 signatures are raw");
            assert_eq!(
                (error.kind, error.message.as_str()),
                (
                    SignerErrorKind::InvalidRequest,
                    "Ed25519 signatures require raw encoding"
                )
            );
        }
    }
}
