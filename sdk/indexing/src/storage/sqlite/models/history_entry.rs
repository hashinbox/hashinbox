//! The `history_entry` view: one address's `transaction_address` rows beside the
//! transactions they name.

use sqlx::{FromRow, SqliteConnection, sqlite::SqliteRow};

use super::{Transaction, TransactionAddress, fetched, framed};
use crate::{IndexError, Network};

/// One transaction in one address's history, before its movements are
/// attached: the `transaction_address` row that places it and the fact it places.
#[derive(Debug)]
pub(in crate::storage::sqlite) struct HistoryEntry {
    address: TransactionAddress,
    transaction: Transaction,
}

impl FromRow<'_, SqliteRow> for HistoryEntry {
    /// The entry `row` holds; the view lends each model the columns of its own
    /// table.
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            address: TransactionAddress::from_row(row)?,
            transaction: Transaction::from_row(row)?,
        })
    }
}

impl HistoryEntry {
    /// The statement [`Self::find_after`] runs.
    pub(in crate::storage::sqlite) const AFTER: &str = "SELECT * FROM history_entry
         WHERE chain = ?1 AND network = ?2 AND address = ?3
           AND (block_height, transaction_order, transaction_id) > (?4, ?5, ?6)
         ORDER BY block_height, transaction_order, transaction_id LIMIT ?7";

    /// Up to `limit` of `address`'s history entries after `after`, in native
    /// block order, their movements not attached.
    pub(in crate::storage::sqlite) async fn find_after(
        connection: &mut SqliteConnection,
        network: &Network,
        address: &str,
        after: &TransactionAddress,
        limit: usize,
    ) -> Result<Vec<Self>, IndexError> {
        let bound = i64::try_from(limit).unwrap_or(i64::MAX);
        let query = framed(Self::AFTER, network)
            .bind(address)
            .bind(after.block_height)
            .bind(after.transaction_order)
            .bind(&after.transaction_id)
            .bind(bound);
        fetched(connection, query, "failed to read history").await
    }
}

impl TryFrom<HistoryEntry> for (TransactionAddress, crate::Transaction) {
    type Error = IndexError;

    /// Where the entry sits in the address's history, and what it holds.
    fn try_from(entry: HistoryEntry) -> Result<Self, IndexError> {
        Ok((entry.address, entry.transaction.try_into()?))
    }
}
