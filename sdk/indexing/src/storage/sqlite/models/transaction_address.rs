//! `transaction_address`: which addresses took part in which transaction, in
//! the order one address's history is paged.

use sqlx::{FromRow, QueryBuilder, SqliteConnection, sqlite::SqliteRow};

use super::{Chunks, column, deleted, network, stored};
use crate::{IndexError, IndexedBatch, Network};

/// One `transaction_address` row: `address` took part in the transaction at
/// `(height, transaction order, transaction id)`, which is the order
/// `transaction_address` is keyed and paged in.
///
/// Every key component is a function of the block alone, which is what makes a
/// page append-only: the order is the transaction's *native* place in its block
/// ([`crate::Transaction::order`]), supplied by the
/// interpreter that held the whole block-ordered list, so a later watch that
/// causes more of the same block to be stored gives each transaction the very
/// place it already had rather than renumbering rows beneath a cursor somebody
/// is holding. A rank this file computed could not: it is handed the
/// transactions that touched one selected address and never the block's list.
#[derive(Clone, Debug)]
pub(in crate::storage::sqlite) struct TransactionAddress {
    network: Network,
    address: String,
    /// Height of the including block.
    pub(in crate::storage::sqlite) block_height: i64,
    /// The transaction's native place in that block.
    pub(in crate::storage::sqlite) transaction_order: i64,
    /// The transaction's own identifier, or the empty sentinel for a row no
    /// transaction names.
    pub(in crate::storage::sqlite) transaction_id: String,
}

impl FromRow<'_, SqliteRow> for TransactionAddress {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            network: network(row)?,
            address: column(row, "address")?,
            block_height: column(row, "block_height")?,
            transaction_order: column(row, "transaction_order")?,
            transaction_id: column(row, "transaction_id")?,
        })
    }
}

impl TransactionAddress {
    /// Every row `batch` writes: one per address that took part in each of
    /// its transactions, block by block.
    pub(in crate::storage::sqlite) fn from_batch(
        batch: &IndexedBatch,
    ) -> Result<Vec<Self>, IndexError> {
        let mut rows = Vec::new();
        for canonical in batch.blocks() {
            for entry in canonical.block.transactions() {
                let block_height = stored(entry.block().height.0)?;
                let transaction_order = stored(entry.order)?;
                rows.extend(entry.addresses().iter().map(|address| Self {
                    network: address.network.clone(),
                    address: address.value.clone(),
                    block_height,
                    transaction_order,
                    transaction_id: entry.id.id.as_str().to_owned(),
                }));
            }
        }
        Ok(rows)
    }

    /// Records every row, many to a statement; one already held is
    /// the same row, because the key is the whole row.
    pub(in crate::storage::sqlite) async fn insert_many(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<(), IndexError> {
        Chunks::new(rows, "failed to write transaction addresses")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT OR IGNORE INTO transaction_address
                         (chain, network, address, block_height, transaction_order,
                          transaction_id) ",
                );
                statement.push_values(chunk, |mut values, row| {
                    values
                        .push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.address.as_str())
                        .push_bind(row.block_height)
                        .push_bind(row.transaction_order)
                        .push_bind(row.transaction_id.as_str());
                });
                statement
            })
            .await
            .map(drop)
    }

    /// Removes every row of `network`; how many rows that was.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<u64, IndexError> {
        deleted(
            connection,
            network,
            "DELETE FROM transaction_address WHERE chain = ?1 AND network = ?2",
        )
        .await
    }
}
