//! The keys this process can sign with, by the id the routes minted.

use std::sync::Arc;

/// The wallets this process holds keys for, by the id it minted.
///
/// It is deliberately here and not in `wallets`: a container there called
/// anything at all would be `Wallets` renamed, which is what `docs/design/030 §8 (ex-029)`
/// removes. What is left once publication, persistence, custody and filters
/// belong to the index is a map with three methods.
#[derive(Default)]
pub struct Signers(std::sync::RwLock<std::collections::BTreeMap<String, Arc<dyn wallets::Wallet>>>);

impl Signers {
    /// An empty key store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Files `wallet` under `id`.
    ///
    /// Call it *after* `index.watch` has committed the row: a crash between
    /// the two then leaves a watched address with no key, which is exactly a
    /// watch-only address, rather than a key nothing observes
    /// (`docs/design/030 §8 (ex-029)` section 3.3).
    ///
    /// # Errors
    ///
    /// [`wallets::WalletErrorKind::Duplicate`] (409) when the id is taken, and
    /// [`wallets::WalletErrorKind::Unavailable`] (503) when the lock is poisoned.
    pub fn keep(
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
    pub fn get(&self, id: &str) -> Option<Arc<dyn wallets::Wallet>> {
        self.0.read().ok().and_then(|held| held.get(id).cloned())
    }

    /// Every id held, in order.
    ///
    /// # Errors
    ///
    /// [`wallets::WalletErrorKind::Unavailable`] (503) when the lock is poisoned.
    pub fn ids(&self) -> Result<Vec<String>, wallets::WalletError> {
        let held = self.0.read().map_err(|_| Self::poisoned())?;
        Ok(held.keys().cloned().collect())
    }

    fn poisoned() -> wallets::WalletError {
        wallets::WalletError::new(
            wallets::WalletErrorKind::Unavailable,
            "the wallet key store is unavailable",
        )
    }
}
