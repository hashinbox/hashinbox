//! `network_checkpoint`: the block one network's confirmed rows are complete
//! to.

use sqlx::{FromRow, SqliteConnection};

use super::{Block, failure};
use crate::{BlockRef, IndexError, Network};

/// One network's checkpoint row, with the block it names joined in.
#[derive(Debug, FromRow)]
pub(in crate::storage::sqlite) struct NetworkCheckpoint {
    #[sqlx(flatten)]
    block: Block,
}

impl NetworkCheckpoint {
    /// The network's checkpoint row; `RowNotFound` before its first commit.
    pub(in crate::storage::sqlite) async fn get(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(
            "SELECT b.position AS block_position, b.height AS block_height,
                    b.hash AS block_hash, b.parents AS block_parents,
                    b.timestamp AS block_timestamp
             FROM network_checkpoint c JOIN block b ON b.id = c.block_id
             WHERE c.chain = ?1 AND c.network = ?2",
        )
        .bind(network.chain.as_str())
        .bind(&network.name)
        .fetch_one(connection)
        .await
    }

    /// Makes `block`, a stored block of `network`, its checkpoint, whether or
    /// not it had one.
    ///
    /// No condition and no version: the caller's write transaction already
    /// holds the file's write lock, and the read this write answers happened
    /// inside it.
    pub(in crate::storage::sqlite) async fn upsert(
        connection: &mut SqliteConnection,
        network: &Network,
        block: &Block,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "INSERT INTO network_checkpoint (chain, network, block_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (chain, network) DO UPDATE SET block_id = excluded.block_id",
        )
        .bind(network.chain.as_str())
        .bind(network.name.as_str())
        .bind(block.id(network))
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to write the checkpoint"))
    }
}

impl TryFrom<NetworkCheckpoint> for BlockRef {
    type Error = IndexError;

    fn try_from(checkpoint: NetworkCheckpoint) -> Result<Self, IndexError> {
        Self::try_from(checkpoint.block)
    }
}
