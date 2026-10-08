//! `block`: every block a stored fact or claim names, once per network and
//! position. Every other table refers to it by `id`, and every read of one of
//! those references joins it and reads the `block_*` columns into an embedded
//! [`Block`].

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sqlx::{FromRow, QueryBuilder, Row as _, SqliteConnection, sqlite::SqliteRow};

use super::{Chunks, canonical_id, column, failure, framed, restored, store, stored};
use crate::{
    BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, IndexError, Network, PurgedRows,
};

/// One block as the `block_*` columns of a join hold it.
///
/// SQLite's only integer is a signed 64-bit one, so the narrowing is named here
/// and checked in both directions rather than cast wherever a block is written
/// or read. `parents` is the column's JSON text, so comparing two blocks is
/// comparing two rows.
#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
pub(in crate::storage::sqlite) struct Block {
    #[sqlx(rename = "block_position")]
    position: i64,
    #[sqlx(rename = "block_height")]
    height: i64,
    #[sqlx(rename = "block_hash")]
    hash: Vec<u8>,
    #[sqlx(rename = "block_parents")]
    parents: String,
    #[sqlx(rename = "block_timestamp")]
    timestamp: Option<i64>,
}

impl Block {
    /// The block a row's `block_*` columns hold, or `None` when its position
    /// is null, which is how a left join says there is none.
    pub(in crate::storage::sqlite) fn optional(
        row: &SqliteRow,
    ) -> Result<Option<Self>, sqlx::Error> {
        if column::<Option<i64>>(row, "block_position")?.is_none() {
            return Ok(None);
        }
        Self::from_row(row).map(Some)
    }

    /// The block's id in `network`: `<chain>:<network>:<hex hash>`.
    pub(in crate::storage::sqlite) fn id(&self, network: &Network) -> String {
        canonical_id(&[
            network.chain.as_str(),
            &network.name,
            &hex::encode(&self.hash),
        ])
    }

    /// Insert-if-absent, verify-if-present, for every block of `blocks`, many
    /// to a statement.
    ///
    /// A block the file already holds under the same id must be the very same
    /// row; another hash at a position the file holds is not written at all and
    /// is found missing. Either is terminal: the chain the file recorded and
    /// the chain being written disagree, which only an explicit recovery of the
    /// network can settle.
    pub(in crate::storage::sqlite) async fn insert_missing(
        connection: &mut SqliteConnection,
        network: &Network,
        blocks: &[Self],
    ) -> Result<(), IndexError> {
        let written = Chunks::new(blocks, "failed to write blocks")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT OR IGNORE INTO block
                         (id, chain, network, hash, position, height, timestamp, parents) ",
                );
                statement.push_values(chunk, |mut values, block: &Self| {
                    values
                        .push_bind(block.id(network))
                        .push_bind(network.chain.as_str())
                        .push_bind(network.name.as_str())
                        .push_bind(block.hash.as_slice())
                        .push_bind(block.position)
                        .push_bind(block.height)
                        .push_bind(block.timestamp)
                        .push_bind(block.parents.as_str());
                });
                statement
            })
            .await?;
        if usize::try_from(written).ok() == Some(blocks.len()) {
            return Ok(());
        }
        let held: BTreeMap<_, _> = Self::find_held(connection, network, blocks)
            .await?
            .into_iter()
            .map(|block| (block.id(network), block))
            .collect();
        if blocks
            .iter()
            .all(|block| held.get(&block.id(network)) == Some(block))
        {
            return Ok(());
        }
        Err(store("a committed block disagrees with the stored one"))
    }

    /// The blocks the file holds under the ids of `blocks`, many to a
    /// statement, in no particular order.
    async fn find_held(
        connection: &mut SqliteConnection,
        network: &Network,
        blocks: &[Self],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(blocks, "failed to look up blocks")
            .fetch(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "SELECT position AS block_position, height AS block_height,
                            hash AS block_hash, parents AS block_parents,
                            timestamp AS block_timestamp
                     FROM block WHERE id IN (",
                );
                let mut ids = statement.separated(", ");
                for block in chunk {
                    ids.push_bind(block.id(network));
                }
                statement.push(")");
                statement
            })
            .await
    }

    /// Deletes every block of `network`, and with them — by cascade — every
    /// transaction, membership, ledger row, output and the checkpoint; every
    /// observation's progress falls back to none. Counts what went first.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<PurgedRows, IndexError> {
        let context = "failed to purge a network";
        let counted = framed(
            "SELECT
                 (SELECT count(*) FROM ledger l JOIN block b ON b.id = l.block_id
                   WHERE b.chain = ?1 AND b.network = ?2
                     AND l.transaction_id IS NULL) AS credits,
                 (SELECT count(*) FROM ledger l JOIN block b ON b.id = l.block_id
                   WHERE b.chain = ?1 AND b.network = ?2
                     AND l.transaction_id IS NOT NULL) AS movements,
                 (SELECT count(*) FROM transaction_address a
                   JOIN \"transaction\" t ON t.id = a.transaction_id
                   JOIN block b ON b.id = t.block_id
                   WHERE b.chain = ?1 AND b.network = ?2) AS history,
                 (SELECT count(*) FROM output
                   WHERE chain = ?1 AND network = ?2) AS outputs,
                 (SELECT count(*) FROM network_checkpoint
                   WHERE chain = ?1 AND network = ?2) AS checkpoint",
            network,
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| failure(&error, context))?;
        let count = |name: &str| {
            counted
                .try_get::<i64, _>(name)
                .map_err(|error| failure(&error, context))
                .and_then(restored)
        };
        let purged = PurgedRows {
            credits: count("credits")?,
            movements: count("movements")?,
            history: count("history")?,
            outputs: count("outputs")?,
            checkpoint: count("checkpoint")?,
        };
        framed(
            "DELETE FROM block WHERE chain = ?1 AND network = ?2",
            network,
        )
        .execute(connection)
        .await
        .map_err(|error| failure(&error, context))?;
        Ok(purged)
    }
}

impl TryFrom<&BlockRef> for Block {
    type Error = IndexError;

    fn try_from(block: &BlockRef) -> Result<Self, IndexError> {
        let parents = match &block.parent {
            Some(parent) => json!([{
                "position": parent.position.0,
                "hash": hex::encode(&parent.hash.0),
            }]),
            None => json!([]),
        };
        Ok(Self {
            position: stored(block.position.0)?,
            height: stored(block.height.0)?,
            hash: block.hash.0.clone(),
            parents: parents.to_string(),
            timestamp: block.timestamp.map(stored).transpose()?,
        })
    }
}

impl TryFrom<Block> for BlockRef {
    type Error = IndexError;

    fn try_from(block: Block) -> Result<Self, IndexError> {
        let malformed = || store("a stored block's parents are malformed");
        let parents: Value = serde_json::from_str(&block.parents).map_err(|_| malformed())?;
        let parent = match parents.as_array().map(Vec::as_slice) {
            Some([]) => None,
            Some([parent]) => {
                let position = parent["position"].as_u64().ok_or_else(malformed)?;
                let hash = parent["hash"].as_str().ok_or_else(malformed)?;
                Some(BlockParent {
                    position: BlockPosition(position),
                    hash: BlockHash(hex::decode(hash).map_err(|_| malformed())?),
                })
            }
            _ => return Err(malformed()),
        };
        Ok(Self {
            position: BlockPosition(restored(block.position)?),
            height: BlockHeight(restored(block.height)?),
            hash: BlockHash(block.hash),
            parent,
            timestamp: block.timestamp.map(restored).transpose()?,
        })
    }
}
