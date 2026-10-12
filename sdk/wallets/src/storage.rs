//! Where an application keeps its wallets: one address of one network each,
//! with the label and key it files for it ([`WalletRepository`]).
//!
//! The `wallet` table is this crate's; indexing observes a wallet through its
//! own `wallet_index` row, which the shared schema ties to the wallet row by a
//! foreign key and which no code here reads. So a wallet is filed here first,
//! then watched; and archiving is the application's sequence — unwatch every
//! asset, then [`WalletRepository::archive`].

use std::fmt;

use base::{Address, KeyFormat};
use crypto::SecretBytes;

use crate::FutureResult;
#[cfg(feature = "sqlite")]
use crate::{WalletError, WalletErrorKind};

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// A private key as a wallet row holds it: the bytes, and what they are.
///
/// Neither [`Clone`] nor serializable; [`Debug`] names the format and never
/// the bytes.
pub struct PrivateKey {
    /// What the bytes are.
    pub format: KeyFormat,
    /// The bytes, zeroized on drop.
    pub secret: SecretBytes,
}

/// The bytes are never formatted.
impl fmt::Debug for PrivateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateKey")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// What an application files for one wallet: its id, its address, and the
/// optional label and key kept beside it. No asset and no birthday: what is
/// observed of the wallet is the index's watch.
#[derive(Debug)]
pub struct Registration {
    /// The caller's id, unique across the store.
    pub id: String,
    /// The address, and therefore the network.
    pub address: Address,
    /// The application's name for the wallet, if any.
    pub label: Option<String>,
    /// The wallet's key, if it has one; `None` files a watch-only wallet.
    pub key: Option<PrivateKey>,
}

/// One stored wallet: a [`Registration`] and whether it is archived.
///
/// Neither [`Clone`] nor serializable, because it may own a key.
#[derive(Debug)]
pub struct StoredWallet {
    registration: Registration,
    archived_at: Option<u64>,
}

impl StoredWallet {
    /// `registration` as a store holds it, archived at that Unix second when
    /// `archived_at` is set.
    #[must_use]
    pub const fn new(registration: Registration, archived_at: Option<u64>) -> Self {
        Self {
            registration,
            archived_at,
        }
    }

    /// The wallet's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.registration.id
    }

    /// The wallet's address.
    #[must_use]
    pub const fn address(&self) -> &Address {
        &self.registration.address
    }

    /// The application's label, if any.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.registration.label.as_deref()
    }

    /// The wallet's key; `None` for a watch-only wallet.
    #[must_use]
    pub const fn key(&self) -> Option<&PrivateKey> {
        self.registration.key.as_ref()
    }

    /// Unix seconds it was archived at; `None` while it is active.
    #[must_use]
    pub const fn archived_at(&self) -> Option<u64> {
        self.archived_at
    }
}

/// The application's wallets: every wallet with its label and key, active or
/// archived.
// linter:disable rust/trait-method-count -- doc 055: file, read, list, archive.
pub trait WalletRepository: Send + Sync {
    /// Files `registration`, and returns the wallet now held for its address.
    ///
    /// An unknown address is filed under the given id, active. An address
    /// this id holds answers the stored wallet, so a repeat settles an
    /// uncertain one; its label is replaced when one is given, and a key is
    /// filed when the row has none.
    ///
    /// # Errors
    ///
    /// [`Conflict`](crate::WalletErrorKind::Conflict) for an address another id holds, an id
    /// held for another address, or a key that is not the one the row holds;
    /// [`InvalidAddress`](crate::WalletErrorKind::InvalidAddress) for a blank address;
    /// [`Unavailable`](crate::WalletErrorKind::Unavailable) when the store fails.
    fn register(&self, registration: Registration) -> FutureResult<'_, StoredWallet>;

    /// The wallet `id` names, active or archived.
    fn wallet<'a>(&'a self, id: &'a str) -> FutureResult<'a, Option<StoredWallet>>;

    /// Every wallet, active and archived, in id order.
    fn wallets(&self) -> FutureResult<'_, Vec<StoredWallet>>;

    /// Archives wallet `id`, keeping the time it was first archived at;
    /// `None` when there is no such wallet. Unwatch its assets first.
    fn archive<'a>(&'a self, id: &'a str) -> FutureResult<'a, Option<StoredWallet>>;
}

/// A repository held behind a reference count is a repository.
impl<T: WalletRepository + ?Sized> WalletRepository for std::sync::Arc<T> {
    fn register(&self, registration: Registration) -> FutureResult<'_, StoredWallet> {
        (**self).register(registration)
    }

    fn wallet<'a>(&'a self, id: &'a str) -> FutureResult<'a, Option<StoredWallet>> {
        (**self).wallet(id)
    }

    fn wallets(&self) -> FutureResult<'_, Vec<StoredWallet>> {
        (**self).wallets()
    }

    fn archive<'a>(&'a self, id: &'a str) -> FutureResult<'a, Option<StoredWallet>> {
        (**self).archive(id)
    }
}

/// A name another wallet holds, or a key that is not the row's: not
/// retryable, because the row is the caller's own earlier filing.
#[cfg(feature = "sqlite")]
fn taken(message: &str) -> WalletError {
    WalletError::new(WalletErrorKind::Conflict, message)
}
