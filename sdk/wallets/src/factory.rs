use std::sync::Arc;

use base::Address;
use crypto::SecretBytes;

use crate::{FutureResult, Wallet};

/// Makes the wallets of one network; a chain's `Factory` implements it,
/// and [`crate::Factories::set`] registers it at the composition root.
pub trait Factory: Send + Sync {
    /// A new key in the chain's import format, on its own curve:
    /// [`Factory::from_pk`] builds the wallet from it.
    ///
    /// # Errors
    ///
    /// The chain's key generation failure.
    fn generate(&self) -> FutureResult<'_, SecretBytes>;

    /// The wallet of `secret`: 32 bytes of key material, which every
    /// supported chain writes as 64 hexadecimal characters and
    /// [`crypto::SecretBytes::from_hex`] decodes chain-neutrally.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::InvalidSecret`] when the bytes are not a key
    /// of this chain's curve.
    #[allow(
        clippy::wrong_self_convention,
        reason = "constructs a wallet from a private key; the factory is the receiver"
    )]
    fn from_pk(&self, secret: SecretBytes) -> FutureResult<'_, Arc<dyn Wallet>>;

    /// The canonical [`Address`] of `text` in this factory's network, without
    /// I/O or a key; accepts every textual form the chain defines.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::InvalidAddress`] when `text` is not an
    /// address of this network.
    fn parse_address(&self, text: &str) -> anyhow::Result<Address>;
}

/// A boxed factory is a factory, so a composition root can hold the
/// factories of several chains as `Box<dyn Factory>`.
impl<F: Factory + ?Sized> Factory for Box<F> {
    fn generate(&self) -> FutureResult<'_, SecretBytes> {
        (**self).generate()
    }

    fn from_pk(&self, secret: SecretBytes) -> FutureResult<'_, Arc<dyn Wallet>> {
        (**self).from_pk(secret)
    }

    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        (**self).parse_address(text)
    }
}
