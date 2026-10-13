//! The `wallet` rows: [`WalletsStore`], which files and restores keyed
//! [`Wallet`]s.
//!
//! A wallet is filed before indexing watches it (the schema's foreign key).
//! Reads rebuild each chain wallet from its stored key through
//! [`Factories::from_pk`], so the store returns [`Wallet`]s, never keys.
//! Anything else the application keeps on its rows (labels, archiving,
//! keyless watched addresses) is the application's own.

use std::sync::Arc;

use base::Address;

use crate::{Factories, FutureResult, Wallet};

/// The application's keyed wallets.
pub trait WalletsStore: Send + Sync {
    /// Files `wallet`'s address and key. A repeat files the key on a row that
    /// lacks one.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::InvalidAddress`] for a blank address; a
    /// different key than the row holds, or the store's failure.
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> FutureResult<'a, ()>;

    /// The keyed wallet of `address`; `None` without one, when the store
    /// withholds it, or when [`Factories`] does not serve its network.
    fn get_by_address<'a>(
        &'a self,
        address: &'a Address,
        factories: &'a Factories,
    ) -> FutureResult<'a, Option<Arc<dyn Wallet>>>;

    /// Every keyed wallet [`Factories`] serves, in address order.
    fn find_all<'a>(&'a self, factories: &'a Factories) -> FutureResult<'a, Vec<Arc<dyn Wallet>>>;
}

impl<T: WalletsStore + ?Sized> WalletsStore for Arc<T> {
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> FutureResult<'a, ()> {
        (**self).insert(wallet)
    }

    fn get_by_address<'a>(
        &'a self,
        address: &'a Address,
        factories: &'a Factories,
    ) -> FutureResult<'a, Option<Arc<dyn Wallet>>> {
        (**self).get_by_address(address, factories)
    }

    fn find_all<'a>(&'a self, factories: &'a Factories) -> FutureResult<'a, Vec<Arc<dyn Wallet>>> {
        (**self).find_all(factories)
    }
}
