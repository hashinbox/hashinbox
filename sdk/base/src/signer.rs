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
