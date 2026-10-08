//! `"transaction"`: one transaction's shared fact, stored once per network
//! whatever number of addresses took part in it.

use std::collections::BTreeMap;

use sqlx::{FromRow, QueryBuilder, SqliteConnection, sqlite::SqliteRow};

use super::{
    Block, Chunks, amount, column, deleted, fetched, framed, network, refused, restored, store,
    stored,
};
use crate::{
    Address, AssetId, IndexError, IndexedBatch, Network, NetworkFee, Status, TransactionRef,
};

/// How the transaction executed: `execution_status` and `failure_reason`.
#[derive(Debug, PartialEq, Eq)]
enum Execution {
    /// Stored as `0`.
    Included,
    /// Stored as `1`, with the chain's reason when it gave one.
    Failed(Option<String>),
}

/// One transaction row: the fact without its movements, which are the
/// `ledger`'s, and with its fee split into three nullable columns.
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
    fee_asset: Option<String>,
    fee_amount: Option<String>,
    fee_payer: Option<String>,
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
            fee_asset: column(row, "fee_asset")?,
            fee_amount: column(row, "fee_amount")?,
            fee_payer: column(row, "fee_payer")?,
        })
    }
}

impl Transaction {
    /// What one row is the only row for: its network and identifier.
    pub(in crate::storage::sqlite) fn key(&self) -> (&Network, &str) {
        (&self.network, &self.transaction_id)
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
    /// mismatch is corruption and terminal rather than a benign duplicate. The
    /// comparison is per transaction rather than per participating address,
    /// which is one row and one truth. When the insert wrote every row there
    /// is nothing to compare; otherwise every row is compared with the one the
    /// file holds, and a row this very insert wrote is that row.
    pub(in crate::storage::sqlite) async fn insert_missing(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<(), IndexError> {
        let written = Self::insert_many(connection, rows).await?;
        if usize::try_from(written).ok() == Some(rows.len()) {
            return Ok(());
        }
        let held = Self::find_held(connection, rows).await?;
        let held: BTreeMap<_, _> = held.iter().map(|row| (row.key(), row)).collect();
        if rows.iter().all(|row| held.get(&row.key()) == Some(&row)) {
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
                         (chain, network, transaction_id, block_position, block_height,
                          block_hash, block_parent_position, block_parent_hash,
                          block_timestamp, transaction_order, execution_status,
                          failure_reason, fee_asset, fee_amount, fee_payer) ",
                );
                statement.push_values(chunk, |mut values, row: &Self| {
                    let (status, reason) = match &row.execution {
                        Execution::Included => (0_i64, None),
                        Execution::Failed(reason) => (1, reason.as_deref()),
                    };
                    values
                        .push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.transaction_id.as_str());
                    row.block.push(&mut values);
                    values
                        .push_bind(row.transaction_order)
                        .push_bind(status)
                        .push_bind(reason)
                        .push_bind(row.fee_asset.as_deref())
                        .push_bind(row.fee_amount.as_deref())
                        .push_bind(row.fee_payer.as_deref());
                });
                statement
            })
            .await
    }

    /// The rows the file holds under the identities of `rows`, many to a
    /// statement, in no particular order.
    async fn find_held(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(rows, "failed to look up transactions")
            .fetch(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "SELECT * FROM \"transaction\"
                     WHERE (chain, network, transaction_id) IN (",
                );
                statement.push_values(chunk, |mut key, row: &Self| {
                    key.push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.transaction_id.as_str());
                });
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
        sqlx::query_as(
            "SELECT * FROM \"transaction\"
             WHERE chain = ?1 AND network = ?2 AND transaction_id = ?3",
        )
        .bind(network.chain.as_str())
        .bind(&network.name)
        .bind(id)
        .fetch_one(connection)
        .await
    }

    /// Up to `limit` of `address`'s transactions after `cursor`
    /// (`(height, order, id)`), in native block order: the `history_entry`
    /// view, one row per transaction the address took part in.
    pub(in crate::storage::sqlite) async fn find_after_by_address(
        connection: &mut SqliteConnection,
        network: &Network,
        address: &str,
        cursor: &(i64, i64, String),
        limit: usize,
    ) -> Result<Vec<Self>, IndexError> {
        let query = framed(
            "SELECT * FROM history_entry
             WHERE chain = ?1 AND network = ?2 AND address = ?3
               AND (block_height, transaction_order, transaction_id) > (?4, ?5, ?6)
             ORDER BY block_height, transaction_order, transaction_id LIMIT ?7",
            network,
        )
        .bind(address)
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

    /// Removes every transaction of `network`; how many rows that was.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<u64, IndexError> {
        deleted(
            connection,
            network,
            "DELETE FROM \"transaction\" WHERE chain = ?1 AND network = ?2",
        )
        .await
    }

    /// The three fee columns as the fee they hold.
    fn fee(&self) -> Result<Option<NetworkFee>, IndexError> {
        let (Some(asset), Some(fee)) = (&self.fee_asset, &self.fee_amount) else {
            if self.fee_asset.is_some() || self.fee_amount.is_some() {
                return Err(store("a stored network fee is half written"));
            }
            return Ok(None);
        };
        Ok(Some(NetworkFee {
            asset: AssetId {
                chain: self.network.chain.clone(),
                asset: asset.clone(),
            },
            amount: amount(fee)?,
            payer: self.fee_payer.as_ref().map(|value| Address {
                network: self.network.clone(),
                value: value.clone(),
            }),
        }))
    }
}

impl TryFrom<&crate::Transaction> for Transaction {
    type Error = IndexError;

    /// The row `entry` is stored as; its movements are not part of it.
    fn try_from(entry: &crate::Transaction) -> Result<Self, IndexError> {
        let fee = entry.fee.as_ref();
        Ok(Self {
            network: entry.id.network.clone(),
            transaction_id: entry.id.id.as_str().to_owned(),
            block: Block::try_from(entry.block())?,
            transaction_order: stored(entry.order)?,
            execution: match &entry.status {
                Status::Included { .. } => Execution::Included,
                Status::Failed { reason, .. } => Execution::Failed(reason.clone()),
            },
            fee_asset: fee.map(|fee| fee.asset.asset.clone()),
            fee_amount: fee.map(|fee| fee.amount.to_string()),
            fee_payer: fee
                .and_then(|fee| fee.payer.as_ref())
                .map(|payer| payer.value.clone()),
        })
    }
}

impl TryFrom<Transaction> for crate::Transaction {
    type Error = IndexError;

    /// The transaction this row holds, with no movements attached.
    fn try_from(row: Transaction) -> Result<Self, IndexError> {
        let fee = row.fee()?;
        let block = row.block.try_into()?;
        Ok(Self {
            id: TransactionRef {
                network: row.network,
                id: row.transaction_id.into(),
            },
            movements: Vec::new(),
            fee,
            order: restored(row.transaction_order)?,
            status: match row.execution {
                Execution::Included => Status::Included { block },
                Execution::Failed(reason) => Status::Failed { block, reason },
            },
        })
    }
}
