//! The keys this process can sign with, by the id the routes minted.

use std::sync::Arc;

/// The wallets this process holds keys for, by the id it minted.
///
/// Application custody stays outside `wallets` and indexing. The encrypted
/// registry restores these handles at boot; durable watches supply public identity.
#[derive(Default)]
pub(crate) struct Signers(
    std::sync::RwLock<std::collections::BTreeMap<String, Arc<dyn wallets::Wallet>>>,
);

impl Signers {
    /// An empty key store.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Files `wallet` under `id`.
    ///
    /// The registry already holds the key and the watch is committed. On restart
    /// those two durable facts reconstruct this in-memory signing handle.
    ///
    /// # Errors
    ///
    /// [`wallets::WalletErrorKind::Duplicate`] (409) when the id is taken, and
    /// [`wallets::WalletErrorKind::Unavailable`] (503) when the lock is poisoned.
    pub(crate) fn keep(
        &self,
        id: String,
        wallet: Arc<dyn wallets::Wallet>,
    ) -> Result<(), wallets::WalletError> {
        let mut held = self.0.write().map_err(|_| Self::poisoned())?;
        if held.contains_key(&id) {
            return Err(wallets::WalletError::new(
                wallets::WalletErrorKind::Duplicate,
                "a wallet is already registered for this key",
            ));
        }
        held.insert(id, wallet);
        Ok(())
    }

    /// The wallet of `id`, or `None` when this process holds no key for it.
    #[must_use]
    pub(crate) fn get(&self, id: &str) -> Option<Arc<dyn wallets::Wallet>> {
        self.0.read().ok().and_then(|held| held.get(id).cloned())
    }

    fn poisoned() -> wallets::WalletError {
        wallets::WalletError::new(
            wallets::WalletErrorKind::Unavailable,
            "the wallet key store is unavailable",
        )
    }
}
