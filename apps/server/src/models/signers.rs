//! The keys this process signs with: every observed, keyed wallet of a served
//! network, rebuilt once at startup from the repository's wallet rows
//! (`indexing::WalletRegistry`), and every wallet created since.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use wallets::{SecretBytes, WalletError, WalletErrorKind};

use crate::asset::Wallets;

/// Every key-holding wallet this process can sign for, by wallet id.
#[derive(Default)]
pub(crate) struct Signers(RwLock<BTreeMap<String, Arc<dyn wallets::Wallet>>>);

impl Signers {
    /// Startup: every observed wallet of a `served` network that holds a key
    /// and whose asset is configured, rebuilt by that asset's factory. Archived
    /// wallets sign nothing, and wallets of networks this process does not
    /// serve wait for a start that serves them. Nothing is watched or
    /// unwatched here: the index restores its watch list from the same rows.
    ///
    /// # Errors
    ///
    /// The registry's read, a factory's refusal of a stored key, and
    /// [`WalletErrorKind::Unavailable`] when a rebuilt key is not its
    /// wallet's address.
    pub(crate) async fn load(
        registry: &dyn indexing::WalletRegistry,
        wallets: &Wallets,
        served: &[base::Network],
    ) -> Result<Self, WalletError> {
        use indexing::Holder as _;
        let stored = registry.wallets().await.map_err(|error| {
            WalletError::new(
                WalletErrorKind::Unavailable,
                format!("wallet registry: {}", error.message),
            )
        })?;
        let assets = wallets.assets();
        let mut signers = BTreeMap::new();
        for wallet in stored
            .iter()
            .filter(|wallet| wallet.archived_at().is_none())
        {
            let Some(secret) = wallet.secret() else {
                continue;
            };
            let address = wallet.address();
            let configured = assets
                .iter()
                .find(|info| info.network == address.network && info.id == *wallet.asset());
            let Some(info) = configured.filter(|_| served.contains(&address.network)) else {
                continue;
            };
            let signer = wallets
                .from_pk(&info.tag, SecretBytes::new(secret.as_bytes()))
                .await?;
            if signer.address() != address {
                return Err(Self::failure("a stored key is not its wallet's address"));
            }
            signers.insert(wallet.id().to_owned(), signer);
        }
        Ok(Self(RwLock::new(signers)))
    }

    /// Keeps `signer` as wallet `id`'s.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the keys' lock is poisoned.
    pub(crate) fn insert(
        &self,
        id: String,
        signer: Arc<dyn wallets::Wallet>,
    ) -> Result<(), WalletError> {
        let mut signers = self.0.write().map_err(|_| Self::poisoned())?;
        signers.insert(id, signer);
        Ok(())
    }

    /// Forgets wallet `id`'s signer, if this process holds one.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the keys' lock is poisoned.
    pub(crate) fn remove(&self, id: &str) -> Result<(), WalletError> {
        let mut signers = self.0.write().map_err(|_| Self::poisoned())?;
        signers.remove(id);
        Ok(())
    }

    /// Wallet `id`'s signer, if this process holds one.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the keys' lock is poisoned.
    pub(crate) fn get(&self, id: &str) -> Result<Option<Arc<dyn wallets::Wallet>>, WalletError> {
        let signers = self.0.read().map_err(|_| Self::poisoned())?;
        Ok(signers.get(id).cloned())
    }

    fn poisoned() -> WalletError {
        Self::failure("the signing keys are unavailable")
    }

    fn failure(message: &'static str) -> WalletError {
        WalletError::new(WalletErrorKind::Unavailable, message)
    }
}
