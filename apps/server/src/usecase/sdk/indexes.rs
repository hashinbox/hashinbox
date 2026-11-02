//! `WalletIndexStore` on the `wallet_index` model.

use base::{Future, Network, WalletIndex, WalletIndexState, WalletIndexStore};

use super::Stores;
use crate::models::{int, uint, wallet_index};

impl WalletIndexStore for Stores {
    fn insert<'a>(&'a self, index: &'a WalletIndex) -> Future<'a, anyhow::Result<()>> {
        let index = index.clone();
        Box::pin(
            self.db
                .write(move |connection| wallet_index::db::insert(connection, &index)),
        )
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
            self.db
                .read(move |connection| {
                    wallet_index::db::find_next(connection, &network, through)?
                        .map(uint)
                        .transpose()
                })
                .await
        })
    }

    fn find_all<'a>(
        &'a self,
        network: &'a Network,
    ) -> Future<'a, anyhow::Result<Vec<WalletIndex>>> {
        let network = network.clone();
        Box::pin(
            self.db
                .read(move |connection| wallet_index::db::find_all(connection, &network)),
        )
    }

    fn move_birthday<'a>(
        &'a self,
        index: &'a WalletIndex,
        birthday: u64,
    ) -> Future<'a, anyhow::Result<()>> {
        let index = index.clone();
        Box::pin(
            self.db.write(move |connection| {
                wallet_index::db::move_birthday(connection, &index, birthday)
            }),
        )
    }

    fn set_state<'a>(
        &'a self,
        index: &'a WalletIndex,
        state: WalletIndexState,
        reason: Option<&'a str>,
    ) -> Future<'a, anyhow::Result<()>> {
        let (index, reason) = (index.clone(), reason.map(str::to_owned));
        Box::pin(self.db.write(move |connection| {
            wallet_index::db::set_state(
                connection,
                &index.address,
                Some(&index.asset.id),
                state,
                reason.as_deref(),
            )?;
            Ok(())
        }))
    }
}
