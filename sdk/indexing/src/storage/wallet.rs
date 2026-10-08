//! The store's wallets: every row a [`WatchStore`] keeps, observed or not,
//! with the label and key an application may file beside it.
//!
//! **One table, two readers.** Scanning reads the observed rows through
//! [`WatchStore`] and never sees a label or a key; an application reads every
//! row, archived ones included, through [`WalletRegistry`]. A [`Watch`] carries
//! no key, so nothing an index holds or logs can leak one.
//!
//! **A row is observed or archived.** Archived is everything that is not
//! scanned: a wallet withdrawn by [`WatchStore::forget`], and a wallet
//! [`WalletRegistry::register`] filed before anything observed it. Archiving
//! keeps the row, its label, its key and every indexed fact of its holding, so
//! re-adding the address restores *that* wallet — the same id and history —
//! rather than minting another one. An archived pair stays reserved for its own
//! identity.

use std::fmt;

use crypto::SecretBytes;

use crate::{
    Address, AssetId, BlockPosition, BoxFuture, Holder, IndexError, IndexErrorKind, Watch,
    WatchStore,
};

/// What an application files for one wallet: its identity, its holding, and
/// the optional label and key the store keeps beside it.
///
/// No birthday: filing a wallet observes nothing. The observation begins when
/// the index watches the id ([`crate::Watching::watch`]), which is what writes
/// the birthday and reactivates the row.
pub struct Registration {
    /// The caller's id, unique across the store.
    pub id: String,
    /// The address, and therefore the network.
    pub address: Address,
    /// The asset its history and balance are read in.
    pub asset: AssetId,
    /// The application's name for the wallet, if any.
    pub label: Option<String>,
    /// The wallet's key, if it has one; `None` files a watch-only wallet.
    pub secret: Option<SecretBytes>,
}

impl Registration {
    /// Rejects a holding no store can hold rows for.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`](crate::IndexErrorKind::InvalidRequest)
    /// for a blank address or an asset of another chain.
    pub fn validate(&self) -> Result<(), IndexError> {
        crate::storage::subject::require_holding(&self.address, &self.asset)
    }
}

impl Holder for Registration {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &AssetId {
        &self.asset
    }
}

/// The key is never formatted.
impl fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("id", &self.id)
            .field("address", &self.address)
            .field("asset", &self.asset)
            .field("label", &self.label)
            .field("keyed", &self.secret.is_some())
            .finish()
    }
}

/// One stored wallet: a [`Registration`] and how it is observed.
///
/// Neither [`Clone`] nor serializable, because it may own a key; [`Debug`]
/// says whether it holds one and never what it is.
pub struct Wallet {
    registration: Registration,
    birthday: Option<BlockPosition>,
    archived_at: Option<u64>,
}

impl Wallet {
    /// `registration` as a store holds it: observed from `birthday` while
    /// `archived_at` is `None`, archived at that Unix second otherwise. An
    /// archived wallet keeps the birthday it was observed from, or has none
    /// when nothing ever observed it.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`], not retryable, for an observed wallet with
    /// no birthday: no store can hold one.
    pub fn new(
        registration: Registration,
        birthday: Option<BlockPosition>,
        archived_at: Option<u64>,
    ) -> Result<Self, IndexError> {
        if archived_at.is_none() && birthday.is_none() {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "an observed wallet has no birthday",
                false,
            ));
        }
        Ok(Self {
            registration,
            birthday,
            archived_at,
        })
    }

    /// The wallet's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.registration.id
    }

    /// The application's label, if any.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.registration.label.as_deref()
    }

    /// The wallet's key; `None` for a watch-only wallet.
    #[must_use]
    pub const fn secret(&self) -> Option<&SecretBytes> {
        self.registration.secret.as_ref()
    }

    /// The first block observed for it; `None` only for a wallet nothing ever
    /// observed.
    #[must_use]
    pub const fn birthday(&self) -> Option<BlockPosition> {
        self.birthday
    }

    /// Unix seconds it was archived at; `None` while it is observed.
    #[must_use]
    pub const fn archived_at(&self) -> Option<u64> {
        self.archived_at
    }

    /// The observation scanning reads, or `None` while archived.
    #[must_use]
    pub fn watch(&self) -> Option<Watch> {
        if self.archived_at.is_some() {
            return None;
        }
        Some(Watch {
            id: self.registration.id.clone(),
            address: self.registration.address.clone(),
            asset: self.registration.asset.clone(),
            start: self.birthday?,
        })
    }
}

impl Wallet {
    /// Rejects a holding no store can hold rows for.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`](crate::IndexErrorKind::InvalidRequest)
    /// for a blank address or an asset of another chain.
    pub fn validate(&self) -> Result<(), IndexError> {
        crate::storage::subject::require_holding(
            &self.registration.address,
            &self.registration.asset,
        )
    }
}

impl Holder for Wallet {
    fn address(&self) -> &Address {
        &self.registration.address
    }

    fn asset(&self) -> &AssetId {
        &self.registration.asset
    }
}

/// The key is never formatted.
impl fmt::Debug for Wallet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Wallet")
            .field("registration", &self.registration)
            .field("birthday", &self.birthday)
            .field("archived_at", &self.archived_at)
            .finish()
    }
}

/// The application's view of a [`WatchStore`]: every wallet it holds, with
/// label and key, observed or archived.
///
/// An optional capability: an index needs only [`WatchStore`], and a store
/// whose application keeps keys elsewhere need not offer this. Archiving is
/// the supertrait's [`WatchStore::forget`]; restoring is
/// [`Self::register`] followed by [`crate::Watching::watch`].
pub trait WalletRegistry: WatchStore {
    /// Files `registration`, and returns the wallet the store now holds for its
    /// `(address, asset)` pair.
    ///
    /// - **An unknown pair** is filed **archived**, never observed: no
    ///   birthday, nothing scanned, its key durable before anything observes
    ///   it. Watching the id ([`crate::Watching::watch`]) then reactivates it.
    /// - **A pair this id holds** answers the stored wallet, so a repeat
    ///   settles an uncertain one.
    /// - **An archived pair** is **restored** under whatever id the call
    ///   names: the stored wallet comes back with its **original id** and
    ///   birthday, still archived until watched again from that birthday.
    ///
    /// For either of the last two the label is replaced when one is given, and
    /// a key is filed when the row has none.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Conflict`], not retryable, for a pair another id
    /// observes, an id held for another pair or network, and a key that is not
    /// the one the row holds; [`IndexErrorKind::InvalidRequest`] for a blank
    /// address or an asset of another chain; and the store's own error.
    fn register<'a>(
        &'a self,
        registration: Registration,
    ) -> BoxFuture<'a, Result<Wallet, IndexError>>;

    /// The wallet `id` names, in whichever network, observed or archived.
    fn wallet<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Wallet>, IndexError>>;

    /// Every wallet of every network, observed and archived, in id order.
    fn wallets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Wallet>, IndexError>>;
}

/// A registry held behind a reference count is a registry.
impl<T: WalletRegistry + ?Sized> WalletRegistry for std::sync::Arc<T> {
    fn register<'a>(
        &'a self,
        registration: Registration,
    ) -> BoxFuture<'a, Result<Wallet, IndexError>> {
        (**self).register(registration)
    }

    fn wallet<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Wallet>, IndexError>> {
        (**self).wallet(id)
    }

    fn wallets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Wallet>, IndexError>> {
        (**self).wallets()
    }
}

/// A key that is not the one the row holds: not retryable, because the row is
/// the caller's own earlier filing. Both shipped stores answer it with this one
/// value.
#[cfg(any(feature = "sqlite", feature = "memory"))]
pub(crate) fn another_key() -> IndexError {
    IndexError::new(
        IndexErrorKind::Conflict,
        "this wallet is registered with another key",
        false,
    )
}

/// The current Unix second, the time an archive is stamped with.
#[cfg(any(feature = "sqlite", feature = "memory"))]
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
