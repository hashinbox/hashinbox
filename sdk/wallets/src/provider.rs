use std::sync::Arc;

use crypto::SecretBytes;
use indexing::IndexScope;

use crate::{Error, FutureResult, Wallet};

/// Constructs one concrete wallet family selected during application startup.
///
/// Every provider must select its native key-generation policy explicitly. An
/// implementation that supplies only the import path must not compile:
///
/// ```compile_fail,E0046
/// use std::sync::Arc;
/// use wallets::{FutureResult, Provider, SecretBytes, Wallet};
///
/// struct IncompleteProvider;
///
/// impl Provider for IncompleteProvider {
///     fn validate(&self, _: &indexing::IndexScope, _: &dyn Wallet) -> Result<(), wallets::Error> {
///         Ok(())
///     }
///
///     fn create<'a>(
///         &'a self,
///         _secret: SecretBytes,
///     ) -> FutureResult<'a, Arc<dyn Wallet>> {
///         unimplemented!()
///     }
/// }
/// ```
pub trait Provider: Send + Sync {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>>;

    /// Generates a wallet without exposing its private key to the caller.
    fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>>;

    /// Checks an existing wallet before collection insertion, without I/O.
    ///
    /// Validate the registered scope, native configuration, and operational
    /// dependencies so single and batch sends use the same family semantics.
    /// Address equality alone does not establish wallet compatibility.
    fn validate(&self, scope: &IndexScope, wallet: &dyn Wallet) -> Result<(), Error>;
}

impl<T: Provider + ?Sized> Provider for Arc<T> {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        (**self).create(secret)
    }

    fn generate(&self) -> FutureResult<'_, Arc<dyn Wallet>> {
        (**self).generate()
    }

    fn validate(&self, scope: &IndexScope, wallet: &dyn Wallet) -> Result<(), Error> {
        (**self).validate(scope, wallet)
    }
}
