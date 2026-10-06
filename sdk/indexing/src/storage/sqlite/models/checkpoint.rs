//! `network_checkpoint`: the line one network's confirmed rows are complete to.

use sqlx::{FromRow, SqliteConnection};

use super::{Block, deleted, failure};
use crate::{BlockRef, IndexError, Network};

/// One network's checkpoint row: the network's two key columns and the
/// block it is complete to.
#[derive(Debug, FromRow)]
pub(in crate::storage::sqlite) struct NetworkCheckpoint {
    chain: String,
    network: String,
    #[sqlx(flatten)]
    block: Block,
}

impl NetworkCheckpoint {
    /// The network's checkpoint row; `RowNotFound` before its first commit.
    pub(in crate::storage::sqlite) async fn get(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as("SELECT * FROM network_checkpoint WHERE chain = ?1 AND network = ?2")
            .bind(network.chain.as_str())
            .bind(&network.name)
            .fetch_one(connection)
            .await
    }

    /// Makes `checkpoint` its network's checkpoint, whether or not it had one.
    ///
    /// No condition and no version: the caller's write transaction already
    /// holds the file's write lock, and the read this write answers happened
    /// inside it.
    pub(in crate::storage::sqlite) async fn upsert(
        connection: &mut SqliteConnection,
        checkpoint: &Self,
    ) -> Result<(), IndexError> {
        let query = sqlx::query(
            "INSERT INTO network_checkpoint
                 (chain, network, block_position, block_height, block_hash,
                  block_parent_position, block_parent_hash, block_timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (chain, network) DO UPDATE SET
                 block_position = excluded.block_position,
                 block_height = excluded.block_height,
                 block_hash = excluded.block_hash,
                 block_parent_position = excluded.block_parent_position,
                 block_parent_hash = excluded.block_parent_hash,
                 block_timestamp = excluded.block_timestamp",
        )
        .bind(checkpoint.chain.as_str())
        .bind(checkpoint.network.as_str());
        checkpoint
            .block
            .bind(query)
            .execute(connection)
            .await
            .map(drop)
            .map_err(|error| failure(&error, "failed to write the checkpoint"))
    }

    /// Removes the network's checkpoint; how many rows that was.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<u64, IndexError> {
        deleted(
            connection,
            network,
            "DELETE FROM network_checkpoint WHERE chain = ?1 AND network = ?2",
        )
        .await
    }
}

impl TryFrom<(&Network, &BlockRef)> for NetworkCheckpoint {
    type Error = IndexError;

    fn try_from((network, block): (&Network, &BlockRef)) -> Result<Self, IndexError> {
        Ok(Self {
            chain: network.chain.as_str().to_owned(),
            network: network.name.clone(),
            block: Block::try_from(block)?,
        })
    }
}

impl TryFrom<NetworkCheckpoint> for BlockRef {
    type Error = IndexError;

    fn try_from(checkpoint: NetworkCheckpoint) -> Result<Self, IndexError> {
        Self::try_from(checkpoint.block)
    }
}
