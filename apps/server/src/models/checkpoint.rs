//! A network's confirmed line, read from indexing's `network_checkpoint` row.

use base::{BlockRef, Network};
use indexing::{IndexError, IndexErrorKind};
use sqlx::SqliteConnection;

use super::Block;

/// The line a read's rows were committed up to.
pub(crate) struct Checkpoint(pub(crate) Option<BlockRef>);

impl Checkpoint {
    /// `network`'s checkpoint, or `None` before its first commit.
    ///
    /// # Errors
    ///
    /// A retryable `Store` error when the read fails, a terminal one for a
    /// malformed row.
    pub(crate) async fn get(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<Self, IndexError> {
        let row = sqlx::query("SELECT * FROM network_checkpoint WHERE chain = ?1 AND network = ?2")
            .bind(network.chain.as_str())
            .bind(&network.name)
            .fetch_optional(connection)
            .await
            .map_err(|_| {
                IndexError::new(IndexErrorKind::Store, "failed to read the checkpoint", true)
            })?;
        row.as_ref().map(Block::stored).transpose().map(Self)
    }
}
