//! `"transaction"`: one transaction's shared fact, stored once per network
//! whatever number of addresses took part in it. Its fee is the payer's
//! `ledger` row, not a column here.

use std::collections::BTreeMap;

use sqlx::{FromRow, QueryBuilder, SqliteConnection, sqlite::SqliteRow};

use super::{
    Block, Chunks, canonical_id, column, fetched, network, refused, restored, store, stored,
};
use crate::{IndexError, IndexedBatch, Network, Status, TransactionRef};

/// The columns every read of a transaction selects: its row, and its block's
/// network and `block_*` columns.
macro_rules! joined {
    ($tail:literal) => {
        concat!(
            "SELECT t.transaction_id, t.transaction_order, t.execution_status,
                    t.failure_reason, b.chain, b.network,
                    b.position AS block_position, b.height AS block_height,
                    b.hash AS block_hash, b.parents AS block_parents,
                    b.timestamp AS block_timestamp
             FROM \"transaction\" t JOIN block b ON b.id = t.block_id ",
            $tail
        )
    };
}

/// How the transaction executed: `execution_status` and `failure_reason`.
#[derive(Debug, PartialEq, Eq)]
enum Execution {
    /// Stored as `0`.
    Included,
    /// Stored as `1`, with the chain's reason when it gave one.
    Failed(Option<String>),
}

/// One transaction row, with the block it names: the fact without its
/// movements and fee, which are the `ledger`'s.
///
/// Equality is the whole row, so a re-presented transaction is verified by
/// comparing the row it would write with the row the file holds: one
/// `(network, transaction)` cannot honestly have two contents.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::storage::sqlite) struct Transaction {
    network: Network,
    transaction_id: String,
    block: Block,
    transaction_order: i64,
    execution: Execution,
}

impl FromRow<'_, SqliteRow> for Transaction {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let reason = column(row, "failure_reason")?;
        let execution = match column::<i64>(row, "execution_status")? {
            0 => Execution::Included,
            1 => Execution::Failed(reason),
            _ => return Err(refused("a stored transaction has an unknown status")),
        };
        Ok(Self {
            network: network(row)?,
            transaction_id: column(row, "transaction_id")?,
            block: Block::from_row(row)?,
            transaction_order: column(row, "transaction_order")?,
            execution,
        })
    }
}

impl Transaction {
    /// The row's id: `<chain>:<network>:<transaction id>`.
    pub(in crate::storage::sqlite) fn id(&self) -> String {
        canonical_id(&[
            self.network.chain.as_str(),
            &self.network.name,
            &self.transaction_id,
        ])
    }

    /// The row of every transaction `batch` records, block by block.
    pub(in crate::storage::sqlite) fn from_batch(
        batch: &IndexedBatch,
    ) -> Result<Vec<Self>, IndexError> {
        let mut rows = Vec::new();
        for canonical in batch.blocks() {
            for entry in canonical.block.transactions() {
                rows.push(Self::try_from(entry)?);
            }
        }
        Ok(rows)
    }

    /// Writes the shared fact of every transaction of one batch, many to a
    /// statement, verifying every row the file already holds.
    ///
    /// A transaction the file already holds is verified rather than rewritten:
    /// one `(network, transaction)` cannot honestly have two contents, so a
    /// mismatch is corruption and terminal rather than a benign duplicate.
    /// When the insert wrote every row there is nothing to compare; otherwise
    /// every row is compared with the one the file holds, and a row this very
    /// insert wrote is that row. Its block must already be stored.
    pub(in crate::storage::sqlite) async fn insert_missing(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<(), IndexError> {
        let written = Self::insert_many(connection, rows).await?;
        if usize::try_from(written).ok() == Some(rows.len()) {
            return Ok(());
        }
        let held = Self::find_held(connection, rows).await?;
        let held: BTreeMap<_, _> = held.iter().map(|row| (row.id(), row)).collect();
        if rows.iter().all(|row| held.get(&row.id()) == Some(&row)) {
            return Ok(());
        }
        Err(store(
            "a committed transaction disagrees with the stored one",
        ))
    }

    /// Insert-if-absent for every row, many to a statement; the number of rows
    /// written, short of `rows` exactly when the file already held some.
    async fn insert_many(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<u64, IndexError> {
        Chunks::new(rows, "failed to write transactions")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT OR IGNORE INTO \"transaction\"
                         (id, block_id, transaction_id, transaction_order, execution_status,
                          failure_reason) ",
                );
                statement.push_values(chunk, |mut values, row: &Self| {
                    let (status, reason) = match &row.execution {
                        Execution::Included => (0_i64, None),
                        Execution::Failed(reason) => (1, reason.as_deref()),
                    };
                    values
                        .push_bind(row.id())
                        .push_bind(row.block.id(&row.network))
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.transaction_order)
                        .push_bind(status)
                        .push_bind(reason);
                });
                statement
            })
            .await
    }

    /// The rows the file holds under the ids of `rows`, many to a statement,
    /// in no particular order.
    async fn find_held(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(rows, "failed to look up transactions")
            .fetch(connection, |chunk| {
                let mut statement = QueryBuilder::new(joined!("WHERE t.id IN ("));
                let mut ids = statement.separated(", ");
                for row in chunk {
                    ids.push_bind(row.id());
                }
                statement.push(")");
                statement
            })
            .await
    }

    /// The row of `id` in `network`.
    pub(in crate::storage::sqlite) async fn get_by_id(
        connection: &mut SqliteConnection,
        network: &Network,
        id: &str,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(joined!("WHERE t.id = ?1"))
            .bind(canonical_id(&[network.chain.as_str(), &network.name, id]))
            .fetch_one(connection)
            .await
    }

    /// Up to `limit` of the transactions of the address `address_id` names
    /// after `cursor` (`(height, order, transaction id)`), in native block
    /// order: the `history_entry` view, one row per transaction the address
    /// took part in.
    pub(in crate::storage::sqlite) async fn find_after_by_address(
        connection: &mut SqliteConnection,
        address_id: &str,
        cursor: &(i64, i64, String),
        limit: usize,
    ) -> Result<Vec<Self>, IndexError> {
        let query = sqlx::query(
            "SELECT * FROM history_entry
             WHERE address_id = ?1
               AND (block_height, transaction_order, transaction_id) > (?2, ?3, ?4)
             ORDER BY block_height, transaction_order, transaction_id LIMIT ?5",
        )
        .bind(address_id)
        .bind(cursor.0)
        .bind(cursor.1)
        .bind(cursor.2.as_str())
        .bind(i64::try_from(limit).unwrap_or(i64::MAX));
        fetched(connection, query, "failed to read history").await
    }

    /// The block that includes this transaction.
    pub(in crate::storage::sqlite) fn block(self) -> Result<crate::BlockRef, IndexError> {
        self.block.try_into()
    }
}

impl TryFrom<&crate::Transaction> for Transaction {
    type Error = IndexError;

    /// The row `entry` is stored as; its movements and fee are not part of it.
    fn try_from(entry: &crate::Transaction) -> Result<Self, IndexError> {
        Ok(Self {
            network: entry.id.network.clone(),
            transaction_id: entry.id.id.as_str().to_owned(),
            block: Block::try_from(entry.block())?,
            transaction_order: stored(entry.order)?,
            execution: match &entry.status {
                Status::Included { .. } => Execution::Included,
                Status::Failed { reason, .. } => Execution::Failed(reason.clone()),
            },
        })
    }
}

impl TryFrom<Transaction> for crate::Transaction {
    type Error = IndexError;

    /// The transaction this row holds, with no movements and no fee attached.
    fn try_from(row: Transaction) -> Result<Self, IndexError> {
        let block = row.block.try_into()?;
        Ok(Self {
            id: TransactionRef {
                network: row.network,
                id: row.transaction_id.into(),
            },
            movements: Vec::new(),
            fee: None,
            order: restored(row.transaction_order)?,
            status: match row.execution {
                Execution::Included => Status::Included { block },
                Execution::Failed(reason) => Status::Failed { block, reason },
            },
        })
    }
}
