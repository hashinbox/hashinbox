//! The SDK signing contract for the native [`KeyPair`].
//!
//! Solana signs exact messages with Ed25519. The key's own curve decides
//! that: `crypto::KeyPair` refuses a digest, a secp256k1 scheme, a key tweak,
//! and any encoding but raw, so this file states no rule of its own.

use base::{SignFuture, SignRequest, Signer};

use crate::native::KeyPair;

impl Signer for KeyPair {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        self.pair.sign(request)
    }
}

#[cfg(test)]
mod tests {
    use base::{
        Curve, Digest, PublicKeyFormat, ScalarTweak, SecretBytes, SignablePayload,
        SignatureEncoding, SignatureScheme, SignerErrorKind,
    };
    use futures_executor::block_on;
    use solana_keypair::{Keypair, Signer as _};

    use super::*;

    const MESSAGE: &[u8] = b"exact Solana wire message";

    fn fixture() -> KeyPair {
        KeyPair::import(SecretBytes::new([7; 32])).expect("fixture key")
    }

    fn request(payload: SignablePayload) -> SignRequest {
        SignRequest {
            payload,
            scheme: SignatureScheme::Ed25519,
            encoding: SignatureEncoding::Raw,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }
    }

    #[test]
    fn a_fixed_seed_and_message_sign_to_the_bytes_the_maintained_library_produces() {
        let key = fixture();
        let signed = block_on(Signer::sign(
            &key,
            request(SignablePayload::Message(MESSAGE.to_vec())),
        ))
        .expect("a whole message must sign");
        let native = Keypair::new_from_array([7; 32]);

        assert_eq!(
            signed.signature.bytes,
            native.sign_message(MESSAGE).as_ref()
        );
        assert_eq!(signed.public_key.bytes, native.pubkey().to_bytes());
        assert_eq!(
            hex::encode(&signed.signature.bytes),
            "53f2c15b62ed217cb3c91d322a67b40adb5f51fb075e0247fd5256ab92e99d87\
             d681bb322cf1c9329975d0e0965abb4a64e4ce0af4c2ae3fb8e0457b1d1c9d0d"
        );
        assert_eq!(
            hex::encode(&signed.public_key.bytes),
            "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
        assert_eq!(signed.public_key.curve, Curve::Ed25519);
        assert_eq!(signed.public_key.bytes, key.address().as_bytes());
    }

    #[test]
    fn implements_the_exact_shared_signer_contract() {
        let key = fixture();
        let mut other_scheme = request(SignablePayload::Message(vec![]));
        other_scheme.scheme = SignatureScheme::EcdsaSecp256k1;
        let mut der = request(SignablePayload::Message(vec![]));
        der.encoding = SignatureEncoding::Der;
        let mut compressed = request(SignablePayload::Message(vec![]));
        compressed.public_key_format = PublicKeyFormat::Compressed;
        let mut tweaked = request(SignablePayload::Message(vec![]));
        tweaked.key_tweak = Some(ScalarTweak::Add([1; 32]));

        for (request, kind, message) in [
            (
                request(SignablePayload::Digest(Digest { bytes: vec![0; 32] })),
                SignerErrorKind::UnsupportedOperation,
                "signer accepts whole messages only",
            ),
            (
                other_scheme,
                SignerErrorKind::UnsupportedScheme,
                "an Ed25519 key signs no secp256k1 request",
            ),
            (
                tweaked,
                SignerErrorKind::UnsupportedOperation,
                "Ed25519 does not accept a key tweak",
            ),
            (
                der,
                SignerErrorKind::InvalidRequest,
                "Ed25519 signatures require raw encoding",
            ),
            (
                compressed,
                SignerErrorKind::InvalidRequest,
                "Ed25519 public keys have one raw 32-byte form",
            ),
        ] {
            let error = block_on(Signer::sign(&key, request)).expect_err("refused request");
            assert_eq!((error.kind, error.message.as_str()), (kind, message));
        }
    }

    #[test]
    fn a_refusal_never_repeats_the_message_it_refused_to_sign() {
        let key = fixture();
        let secret = "private message fixture";
        let mut request = request(SignablePayload::Message(secret.as_bytes().to_vec()));
        request.scheme = SignatureScheme::EcdsaSecp256k1;
        let error = block_on(Signer::sign(&key, request)).expect_err("refused request");

        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }
}
