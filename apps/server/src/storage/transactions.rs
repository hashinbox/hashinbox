//! `base::TransactionStore` on the `transaction` and `leg` models.

use base::{Address, Future, Transaction, TransactionStore};

use super::Storage;
use crate::models::{LegRow, TransactionRow};

impl TransactionStore for Storage {
    fn insert<'a>(&'a self, transaction: &'a Transaction) -> Future<'a, anyhow::Result<()>> {
        let transaction = transaction.clone();
        Box::pin(
            self.write(move |connection| TransactionRow::insert_pending(connection, &transaction)),
        )
    }

    fn find_for_address<'a>(
        &'a self,
        address: &'a Address,
    ) -> Future<'a, anyhow::Result<Vec<Transaction>>> {
        let address = address.clone();
        Box::pin(self.read(move |connection| {
            let mut found = Vec::new();
            for row in TransactionRow::find_for_address(connection, &address)? {
                let legs = LegRow::find_for(connection, &row.id)?;
                found.push(row.transaction(&address.network, legs)?);
            }
            Ok(found)
        }))
    }
}
