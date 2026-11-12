//! The `trigger` package's [`Store`] over SQLite: a network's wallet indexes
//! at its confirmed block, every read inside the caller's one transaction,
//! every statement `wallet_index`'s.

use ::trigger::{
    Store,
    selection::{Index, Selector},
};
use base::{Address, Asset, Future, Network};
use diesel::SqliteConnection;

use crate::models::wallet_index;

/// [`Store`] over one read transaction of `network` at its confirmed
/// position.
pub(crate) struct Sqlite<'c> {
    connection: &'c mut SqliteConnection,
    network: Network,
    confirmed: i64,
}

impl<'c> Sqlite<'c> {
    /// `network`'s indexes over `connection`, read at `confirmed`.
    pub(crate) const fn new(
        connection: &'c mut SqliteConnection,
        network: Network,
        confirmed: i64,
    ) -> Self {
        Self {
            connection,
            network,
            confirmed,
        }
    }
}

impl Store for Sqlite<'_> {
    fn select<'a>(&'a mut self, selector: &'a Selector) -> Future<'a, anyhow::Result<Vec<Index>>> {
        let found =
            wallet_index::db::select(self.connection, &self.network, selector, self.confirmed);
        Box::pin(std::future::ready(found))
    }

    fn find<'a>(
        &'a mut self,
        wanted: &'a [(Address, Asset)],
    ) -> Future<'a, anyhow::Result<Vec<Index>>> {
        let found = wallet_index::db::find(self.connection, &self.network, wanted, self.confirmed);
        Box::pin(std::future::ready(found))
    }
}
