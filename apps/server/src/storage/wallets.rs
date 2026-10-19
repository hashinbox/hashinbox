//! `base::WalletStore` on the `wallet` model: keys restored through
//! `WalletFactories::from_pk`; and the wallet listing's reads.

use std::sync::Arc;

use anyhow::{Context as _, ensure};
use base::{Address, Asset, Future, Wallet, WalletFactories, WalletStore};

use super::Storage;
use crate::models::{self, wallet_index};

impl WalletStore for Storage {
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> Future<'a, anyhow::Result<()>> {
        let address = wallet.address().clone();
        let (curve, secret) = wallet.private_key();
        Box::pin(self.write(move |connection| {
            models::wallet::db::insert(connection, &models::Wallet::new(&address, curve, secret)?)?;
            Ok(())
        }))
    }

    fn find_all<'a>(
        &'a self,
        factories: &'a WalletFactories,
    ) -> Future<'a, anyhow::Result<Vec<Arc<dyn Wallet>>>> {
        Box::pin(async move {
            let rows = self
                .read(|connection| Ok(models::wallet::db::find_all(connection)?))
                .await?;
            let mut wallets = Vec::with_capacity(rows.len());
            for row in rows {
                let address = row.address();
                row.curve()?;
                let wallet = factories
                    .from_pk(&address.network, row.secret.0)
                    .await
                    .with_context(|| format!("the wallet of {address} does not restore"))?;
                ensure!(
                    *wallet.address() == address,
                    "the key kept for {address} restores another address"
                );
                wallets.push(wallet);
            }
            Ok(wallets)
        })
    }

    fn delete<'a>(&'a self, address: &'a Address) -> Future<'a, anyhow::Result<()>> {
        let address = address.clone();
        Box::pin(self.write(move |connection| {
            models::wallet::db::delete(connection, &models::Wallet::id(&address)?)?;
            Ok(())
        }))
    }
}

impl Storage {
    /// The address of every kept wallet; no key is restored.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) async fn keyed(&self) -> anyhow::Result<Vec<Address>> {
        self.read(|connection| Ok(models::wallet::db::find_all(connection)?.addresses()))
            .await
    }

    /// Every watched (address, asset) of every network.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) async fn watched(&self) -> anyhow::Result<Vec<(Address, Asset)>> {
        self.read(|connection| {
            Ok(wallet_index::db::find_every(connection)?
                .into_iter()
                .map(|index| (index.address, index.asset))
                .collect())
        })
        .await
    }
}
