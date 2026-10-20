//! `base::WalletIndexStore` on the `wallet_index` model.

use base::{Future, Network, WalletIndexStore};

use super::Storage;
use crate::models::{int, uint, wallet_index};

impl WalletIndexStore for Storage {
    fn insert<'a>(&'a self, index: &'a base::WalletIndex) -> Future<'a, anyhow::Result<()>> {
        let index = index.clone();
        Box::pin(self.write(move |connection| wallet_index::db::insert(connection, &index)))
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
}
