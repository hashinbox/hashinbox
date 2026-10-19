//! `base::WalletIndexStore` on the `wallet_index` model.

use base::{Address, Asset, Future, Network, WalletIndexStore};

use super::Storage;
use crate::models::{int, uint, wallet_index};

impl WalletIndexStore for Storage {
    fn insert<'a>(&'a self, index: &'a base::WalletIndex) -> Future<'a, anyhow::Result<()>> {
        let index = index.clone();
        Box::pin(self.write(move |connection| wallet_index::db::insert(connection, &index)))
    }

    fn find_all<'a>(
        &'a self,
        network: &'a Network,
    ) -> Future<'a, anyhow::Result<Vec<base::WalletIndex>>> {
        let network = network.clone();
        Box::pin(self.read(move |connection| wallet_index::db::find_all(connection, &network)))
    }

    fn find_next<'a>(
        &'a self,
        network: &'a Network,
        through: Option<u64>,
    ) -> Future<'a, anyhow::Result<Option<u64>>> {
        let network = network.clone();
        Box::pin(async move {
            // Above SQLite's range bounds nothing.
            let through = through.map(|through| int(through).unwrap_or(i64::MAX));
            self.read(move |connection| {
                wallet_index::db::find_next(connection, &network, through)?
                    .map(uint)
                    .transpose()
            })
            .await
        })
    }

    fn delete<'a>(
        &'a self,
        address: &'a Address,
        asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<()>> {
        let (address, asset) = (address.clone(), asset.clone());
        Box::pin(self.write(move |connection| {
            wallet_index::db::delete(connection, &address, &asset)?;
            Ok(())
        }))
    }
}
