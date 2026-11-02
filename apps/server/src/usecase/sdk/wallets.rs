//! `base::WalletStore` on the `wallet` model: keys restored through
//! `WalletFactories::from_pk`.

use std::sync::Arc;

use anyhow::{Context as _, ensure};
use base::{Address, Future, Wallet, WalletFactories, WalletStore};

use super::Stores;
use crate::models;

impl WalletStore for Stores {
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> Future<'a, anyhow::Result<()>> {
        let address = wallet.address().clone();
        let (curve, secret) = wallet.private_key();
        Box::pin(self.db.write(move |connection| {
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
                .db
                .read(|connection| Ok(models::wallet::db::find_all(connection)?))
                .await?;
            let mut wallets = Vec::with_capacity(rows.len());
            for row in rows {
                let address = Address::from(&row);
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
}
