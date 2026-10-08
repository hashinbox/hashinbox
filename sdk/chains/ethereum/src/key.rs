//! The SDK signing contract for the native [`KeyPair`].

use base::{SignFuture, SignRequest, Signer};

use crate::native::KeyPair;

impl Signer for KeyPair {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        self.pair.sign(request)
    }
}
