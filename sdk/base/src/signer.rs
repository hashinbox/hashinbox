use std::{future::Future, pin::Pin};

use crate::{SignRequest, SignedPayload, SignerError};

/// The future a [`Signer`] returns: boxed so the trait stays `dyn`-safe without a runtime.
pub type SignFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SignedPayload, SignerError>> + Send + 'a>>;

/// Minimal cryptographic boundary shared by chain transaction implementations.
///
/// Chains build the request; the signer never sees a transaction, so an
/// in-process [`crypto::KeyPair`] — the engine behind every chain's key, on
/// either curve — and a future remote signer are interchangeable. The request,
/// its payload, and its result are `crypto` values ([`SignRequest`],
/// [`SignedPayload`], [`SignerError`]); the capability is asynchronous because
/// a key may live in an HSM, a KMS, or another process.
pub trait Signer: Send + Sync {
    /// Signs one request.
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a>;
}

impl Signer for crypto::KeyPair {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        Box::pin(async move { self.sign_now(request) })
    }
}

#[cfg(test)]
mod tests {
    use crypto::KeyPair;
    use futures_executor::block_on;

    use super::*;
    use crate::{Digest, PublicKeyFormat, SignablePayload, SignatureEncoding, SignatureScheme};

    #[test]
    fn the_local_key_is_a_signer_and_its_async_result_is_its_own() {
        let pair = KeyPair::secp256k1([1_u8; 32]).expect("test key must be valid");
        let request = SignRequest {
            payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        };
        let signed = block_on(pair.sign(request.clone())).expect("valid digest must sign");

        assert_eq!(signed.signature.bytes.len(), 65);
        assert_eq!(signed.public_key.bytes.len(), 64);
        assert_eq!(signed, pair.sign_now(request).expect("the same request"));
    }

    #[test]
    fn the_same_capability_serves_an_ed25519_key() {
        let pair = KeyPair::ed25519([1_u8; 32]).expect("a 32-byte seed is always valid");
        let request = SignRequest {
            payload: SignablePayload::Message(b"message".to_vec()),
            scheme: SignatureScheme::Ed25519,
            encoding: SignatureEncoding::Raw,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        };
        let signed = block_on(pair.sign(request.clone())).expect("a whole message must sign");

        assert_eq!(signed.signature.bytes.len(), 64);
        assert_eq!(signed.public_key.bytes.len(), 32);
        assert_eq!(signed, pair.sign_now(request).expect("the same request"));
    }
}
