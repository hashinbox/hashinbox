//! `base::WalletStore` on the `wallet` model: only keyed rows, keys restored
//! through `WalletFactories::from_pk`. A watch-only row is not a store's
//! wallet; keeping a key for one fills its row.

use std::sync::Arc;

use anyhow::{Context as _, ensure};
use base::{Addressed as _, Future, Wallet, WalletFactories, WalletStore};

use super::Stores;
use crate::models;

impl WalletStore for Stores {
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> Future<'a, anyhow::Result<()>> {
        let row = models::Wallet::try_from(wallet);
        Box::pin(async move {
            let row = row?;
            self.db
                .write(move |connection| {
                    models::wallet::db::insert(connection, &row)?;
                    Ok(())
                })
                .await
        })
    }

    fn find_all<'a>(
        &'a self,
        factories: &'a WalletFactories,
    ) -> Future<'a, anyhow::Result<Vec<Arc<dyn Wallet>>>> {
        Box::pin(async move {
            let rows = self
                .db
                .read(|connection| Ok(models::wallet::db::find_keyed(connection)?))
                .await?;
            let mut wallets = Vec::with_capacity(rows.len());
            for row in rows {
                let address = row.address().clone();
                row.curve()?;
                let secret = row
                    .secret
                    .0
                    .with_context(|| format!("the wallet of {address} keeps no key"))?;
                let wallet = factories
                    .from_pk(&address.network, secret)
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
