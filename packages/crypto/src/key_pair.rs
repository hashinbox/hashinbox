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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Curve, Digest, ScalarTweak, SignablePayload, SignatureEncoding, SignatureScheme,
        SignerErrorKind,
    };

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn unsupported_signing_requests_preserve_error_kind_and_validation_order() {
        let pair = KeyPair::secp256k1([1_u8; 32]).expect("test key must be valid");
        for (payload, scheme, key_tweak, kind, message) in [
            (
                SignablePayload::Message(vec![9; 32]),
                SignatureScheme::Ed25519,
                None,
                SignerErrorKind::UnsupportedOperation,
                "signer accepts precomputed digests only",
            ),
            (
                SignablePayload::Digest(Digest { bytes: vec![9; 31] }),
                SignatureScheme::EcdsaSecp256k1,
                Some(ScalarTweak::Add([0; 32])),
                SignerErrorKind::UnsupportedOperation,
                "ECDSA does not accept a key tweak",
            ),
            (
                SignablePayload::Digest(Digest { bytes: vec![9; 31] }),
                SignatureScheme::Ed25519,
                None,
                SignerErrorKind::UnsupportedScheme,
                "a secp256k1 key signs no Ed25519 request",
            ),
        ] {
            let error = pair
                .sign_now(SignRequest {
                    payload,
                    scheme,
                    encoding: SignatureEncoding::Recoverable,
                    public_key_format: PublicKeyFormat::Raw,
                    key_tweak,
                })
                .expect_err("unsupported requests must fail before digest signing");
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn primitive_failures_keep_invalid_request_mapping_and_validation_precedence() {
        for rejected in [vec![0_u8; 32], vec![1_u8; 31], vec![1_u8; 33]] {
            let error = KeyPair::secp256k1(rejected).err().unwrap();
            assert_eq!(error.kind, SignerErrorKind::InvalidRequest);
            assert_eq!(
                error.message,
                "secret key must be a valid 32-byte secp256k1 scalar"
            );
        }
        for rejected in [vec![1_u8; 31], vec![1_u8; 33]] {
            let error = KeyPair::ed25519(rejected).err().unwrap();
            assert_eq!(error.kind, SignerErrorKind::InvalidRequest);
            assert_eq!(
                error.message,
                "secret key must be exactly 32 Ed25519 seed bytes"
            );
        }
        let pair = KeyPair::secp256k1([1_u8; 32]).unwrap();
        for (scheme, encoding, size, key_tweak, message) in [
            (
                SignatureScheme::EcdsaSecp256k1,
                SignatureEncoding::Der,
                31,
                None,
                "digest must contain exactly 32 bytes",
            ),
            (
                SignatureScheme::SchnorrSecp256k1,
                SignatureEncoding::Der,
                31,
                None,
                "Schnorr signatures require raw encoding",
            ),
            (
                SignatureScheme::SchnorrSecp256k1,
                SignatureEncoding::Raw,
                31,
                Some(ScalarTweak::TaggedHashAdd {
                    tag: Vec::new(),
                    suffix: Vec::new(),
                }),
                "digest must contain exactly 32 bytes",
            ),
            (
                SignatureScheme::SchnorrSecp256k1,
                SignatureEncoding::Raw,
                32,
                Some(ScalarTweak::TaggedHashAdd {
                    tag: Vec::new(),
                    suffix: Vec::new(),
                }),
                "tag must not be empty",
            ),
        ] {
            let error = pair
                .sign_now(SignRequest {
                    payload: SignablePayload::Digest(Digest {
                        bytes: vec![9; size],
                    }),
                    scheme,
                    encoding,
                    public_key_format: PublicKeyFormat::Raw,
                    key_tweak,
                })
                .unwrap_err();
            assert_eq!(error.kind, SignerErrorKind::InvalidRequest);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn one_key_derives_every_public_key_layout_on_secp256k1() {
        let pair = KeyPair::secp256k1([1_u8; 32]).expect("test key must be valid");
        for (format, length) in [
            (PublicKeyFormat::Compressed, 33),
            (PublicKeyFormat::Uncompressed, 65),
            (PublicKeyFormat::XOnly, 32),
            (PublicKeyFormat::Raw, 64),
        ] {
            let public = pair.public_key(format).expect("the curve accepts it");
            assert_eq!(public.curve, Curve::Secp256k1);
            assert_eq!(public.format, format);
            assert_eq!(public.bytes.len(), length);
        }
    }

    #[test]
    fn the_key_signs_a_digest_under_its_own_public_key() {
        let pair = KeyPair::secp256k1([1_u8; 32]).expect("test key must be valid");
        let signed = pair
            .sign_now(SignRequest {
                payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Recoverable,
                public_key_format: PublicKeyFormat::Raw,
                key_tweak: None,
            })
            .expect("valid digest must sign");

        assert_eq!(signed.signature.bytes.len(), 65);
        assert_eq!(signed.public_key.bytes.len(), 64);
    }

    #[test]
    fn an_ed25519_key_signs_the_exact_message_under_its_raw_public_key() {
        let pair = KeyPair::ed25519([7_u8; 32]).expect("a 32-byte seed is always valid");
        let signed = pair
            .sign_now(SignRequest {
                payload: SignablePayload::Message(b"exact Solana wire message".to_vec()),
                scheme: SignatureScheme::Ed25519,
                encoding: SignatureEncoding::Raw,
                public_key_format: PublicKeyFormat::Raw,
                key_tweak: None,
            })
            .expect("a whole message must sign");

        assert_eq!(signed.public_key.curve, Curve::Ed25519);
        assert_eq!(
            hex(&signed.public_key.bytes),
            "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
        assert_eq!(
            hex(&signed.signature.bytes),
            "53f2c15b62ed217cb3c91d322a67b40adb5f51fb075e0247fd5256ab92e99d87\
             d681bb322cf1c9329975d0e0965abb4a64e4ce0af4c2ae3fb8e0457b1d1c9d0d"
        );
    }
}
