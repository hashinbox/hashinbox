//! `transaction_address`: which addresses took part in which transaction.
//! One address's history pages walk these memberships through the
//! `history_entry` view.

use sqlx::{QueryBuilder, SqliteConnection};

use super::{Birthdays, Chunks, address_id, canonical_id, failure, stored};
use crate::{IndexError, IndexedBatch};

/// One `transaction_address` row: the transaction's id and the id of an
/// address that took part in it. The key is the whole row.
#[derive(Clone, Debug)]
pub(in crate::storage::sqlite) struct TransactionAddress {
    transaction_id: String,
    address_id: String,
}

impl TransactionAddress {
    /// Every row `batch` writes: one per watched address that took part in
    /// each of its transactions, block by block.
    pub(in crate::storage::sqlite) fn from_batch(
        batch: &IndexedBatch,
        watched: &Birthdays,
    ) -> Result<Vec<Self>, IndexError> {
        let mut rows = Vec::new();
        for canonical in batch.blocks() {
            let position = stored(canonical.block.reference().position.0)?;
            for entry in canonical.block.transactions() {
                let network = &entry.id.network;
                let transaction_id =
                    canonical_id(&[network.chain.as_str(), &network.name, entry.id.id.as_str()]);
                let members = entry
                    .addresses()
                    .iter()
                    .map(|address| address_id(&address.network, &address.value))
                    .filter(|member| watched.takes_part(member, position))
                    .map(|member| Self {
                        transaction_id: transaction_id.clone(),
                        address_id: member,
                    })
                    .collect::<Vec<_>>();
                rows.extend(members);
            }
        }
        Ok(rows)
    }

    /// Deletes every membership of `address_id`; the transaction ids they
    /// named.
    pub(in crate::storage::sqlite) async fn delete_by_address(
        connection: &mut SqliteConnection,
        address_id: &str,
    ) -> Result<Vec<String>, IndexError> {
        sqlx::query_scalar(
            "DELETE FROM transaction_address WHERE address_id = ?1 RETURNING transaction_id",
        )
        .bind(address_id)
        .fetch_all(connection)
        .await
        .map_err(|error| failure(&error, "failed to delete memberships"))
    }

    /// Records every row, many to a statement; one already held is the same
    /// row, because the key is the whole row.
    pub(in crate::storage::sqlite) async fn insert_many(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<(), IndexError> {
        Chunks::new(rows, "failed to write transaction addresses")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT OR IGNORE INTO transaction_address (transaction_id, address_id) ",
                );
                statement.push_values(chunk, |mut values, row| {
                    values
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.address_id.as_str());
                });
                statement
            })
            .await
            .map(drop)
    }
}
