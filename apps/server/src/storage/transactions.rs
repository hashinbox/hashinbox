//! `base::TransactionStore` on the `transaction` and `leg` models.

use base::{Future, TransactionStore};

use super::Storage;
use crate::models::transaction;

impl TransactionStore for Storage {
    fn insert<'a>(&'a self, transaction: &'a base::Transaction) -> Future<'a, anyhow::Result<()>> {
        let transaction = transaction.clone();
        Box::pin(
            self.write(move |connection| transaction::db::insert_pending(connection, &transaction)),
        )
    }
}
