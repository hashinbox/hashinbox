//! `base::TransactionStore` on the `transaction` and `leg` models.

use base::{Address, Future, TransactionStore};

use super::Storage;
use crate::models::{leg, transaction};

impl TransactionStore for Storage {
    fn insert<'a>(&'a self, transaction: &'a base::Transaction) -> Future<'a, anyhow::Result<()>> {
        let transaction = transaction.clone();
        Box::pin(
            self.write(move |connection| transaction::db::insert_pending(connection, &transaction)),
        )
    }

    fn find_for_address<'a>(
        &'a self,
        address: &'a Address,
    ) -> Future<'a, anyhow::Result<Vec<base::Transaction>>> {
        let address = address.clone();
        Box::pin(self.read(move |connection| {
            let mut found = Vec::new();
            for row in transaction::db::find_for_address(connection, &address)? {
                let legs = leg::db::find_for(connection, &row.id)?;
                found.push(row.transaction(&address.network, legs)?);
            }
            Ok(found)
        }))
    }
}
