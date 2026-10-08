//! The SDK signing contract for the native [`KeyPair`].
//!
//! A Bitcoin key names no network, so it has no address of its own: the
//! wallet it is attached to derives one from its configuration.

use base::{SignFuture, SignRequest, Signer};

use crate::native::KeyPair;

impl Signer for KeyPair {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        self.pair.sign(request)
    }
}
