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
