//! `base::WalletStore` on the `wallet` model: keys restored through
//! `WalletFactories::from_pk`; and the wallet listing's reads.

use std::sync::Arc;

use anyhow::{Context as _, ensure};
use base::{Address, Asset, Chain, Future, Network, Wallet, WalletFactories, WalletStore};

use super::Storage;
use crate::models::{WalletIndexRow, WalletRow};

impl WalletStore for Storage {
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> Future<'a, anyhow::Result<()>> {
        let address = wallet.address().clone();
        let (curve, secret) = wallet.private_key();
        Box::pin(self.write(move |connection| {
            WalletRow::insert(connection, &address, curve, &secret)?;
            Ok(())
        }))
    }

    fn find_all<'a>(
        &'a self,
        factories: &'a WalletFactories,
    ) -> Future<'a, anyhow::Result<Vec<Arc<dyn Wallet>>>> {
        Box::pin(async move {
            let rows = self
                .read(|connection| Ok(WalletRow::find_all(connection)?))
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
            WalletRow::delete(connection, &address)?;
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
        self.read(|connection| {
            Ok(WalletRow::find_all(connection)?
                .iter()
                .map(WalletRow::address)
                .collect())
        })
        .await
    }

    /// Every watched (address, asset) of every network.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) async fn watched(&self) -> anyhow::Result<Vec<(Address, Asset)>> {
        self.read(|connection| {
            Ok(WalletIndexRow::find_every(connection)?
                .into_iter()
                .map(|row| {
                    let network = Network::new(Chain::new(row.chain), row.network);
                    let asset = Asset {
                        network: network.clone(),
                        id: row.asset,
                    };
                    (Address::new(network, row.address), asset)
                })
                .collect())
        })
        .await
    }
}
