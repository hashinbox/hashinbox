//! The keys this process signs with: every active, keyed wallet of a served
//! network, rebuilt once at startup from the repository's wallet rows
//! (`wallets::WalletRepository`) for every configured asset of its network
//! whose factory takes its key format, and every wallet created since.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use wallets::{SecretBytes, WalletError, WalletErrorKind};

use crate::asset::{AssetTag, Wallets};

/// Every key-holding wallet this process can sign for, by wallet id and the
/// configured asset whose factory built it.
#[derive(Default)]
pub(crate) struct Signers(RwLock<BTreeMap<(String, AssetTag), Arc<dyn wallets::Wallet>>>);

impl Signers {
    /// Startup: every active wallet of a `served` network that holds a key,
    /// rebuilt by each configured asset factory of its network that takes its
    /// key format. Archived wallets sign nothing, and wallets of networks this
    /// process does not serve wait for a start that serves them. Nothing is
    /// watched or unwatched here: the index restores its watch list itself.
    ///
    /// # Errors
    ///
    /// The registry's read, a factory's refusal of a stored key, and
    /// [`WalletErrorKind::Unavailable`] when a rebuilt key is not its
    /// wallet's address.
    pub(crate) async fn load(
        registry: &dyn wallets::WalletRepository,
        wallets: &Wallets,
        served: &[base::Network],
    ) -> Result<Self, WalletError> {
        let stored = registry.wallets().await?;
        let assets = wallets.assets();
        let mut signers = BTreeMap::new();
        for wallet in stored
            .iter()
            .filter(|wallet| wallet.archived_at().is_none())
        {
            let Some(key) = wallet.key() else {
                continue;
            };
            let address = wallet.address();
            if !served.contains(&address.network) {
                continue;
            }
            let configured = assets
                .iter()
                .filter(|info| info.network == address.network && info.key_format == key.format);
            for info in configured {
                let signer = wallets
                    .from_pk(&info.tag, SecretBytes::new(key.secret.as_bytes()))
                    .await?;
                if signer.address() != address {
                    return Err(Self::failure("a stored key is not its wallet's address"));
                }
                signers.insert((wallet.id().to_owned(), info.tag.clone()), signer);
            }
        }
        Ok(Self(RwLock::new(signers)))
    }

    /// Keeps `signer` as wallet `id`'s for asset `tag`.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the keys' lock is poisoned.
    pub(crate) fn insert(
        &self,
        id: String,
        tag: AssetTag,
        signer: Arc<dyn wallets::Wallet>,
    ) -> Result<(), WalletError> {
        let mut signers = self.0.write().map_err(|_| Self::poisoned())?;
        signers.insert((id, tag), signer);
        Ok(())
    }

    /// Forgets every signer of wallet `id` this process holds.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the keys' lock is poisoned.
    pub(crate) fn remove(&self, id: &str) -> Result<(), WalletError> {
        let mut signers = self.0.write().map_err(|_| Self::poisoned())?;
        signers.retain(|(held, _), _| held != id);
        Ok(())
    }

    /// Wallet `id`'s signer for asset `tag`, if this process holds one.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the keys' lock is poisoned.
    pub(crate) fn get(
        &self,
        id: &str,
        tag: &AssetTag,
    ) -> Result<Option<Arc<dyn wallets::Wallet>>, WalletError> {
        let signers = self.0.read().map_err(|_| Self::poisoned())?;
        Ok(signers.get(&(id.to_owned(), tag.clone())).cloned())
    }

    fn poisoned() -> WalletError {
        Self::failure("the signing keys are unavailable")
    }

    fn failure(message: &'static str) -> WalletError {
        WalletError::new(WalletErrorKind::Unavailable, message)
    }
}
