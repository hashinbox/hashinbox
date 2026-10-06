//! The six `block_*` columns every table that names a block holds it as.

use sqlx::{FromRow, Sqlite, query_builder::Separated, sqlite::SqliteRow};

use super::{Bound, column, restored, store, stored};
use crate::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, IndexError};

/// One block reference as six columns hold it.
///
/// SQLite's only integer is a signed 64-bit one, so the narrowing is named here
/// and checked in both directions rather than cast wherever a block is written
/// or read.
#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
pub(in crate::storage::sqlite) struct Block {
    #[sqlx(rename = "block_position")]
    position: i64,
    #[sqlx(rename = "block_height")]
    height: i64,
    #[sqlx(rename = "block_hash")]
    hash: Vec<u8>,
    #[sqlx(rename = "block_parent_position")]
    parent_position: Option<i64>,
    #[sqlx(rename = "block_parent_hash")]
    parent_hash: Option<Vec<u8>>,
    #[sqlx(rename = "block_timestamp")]
    timestamp: Option<i64>,
}

impl Block {
    /// The block a row's six `block_*` columns hold, or `None` when its
    /// position is null, which is how a column set that may be empty says it
    /// is.
    pub(in crate::storage::sqlite) fn optional(
        row: &SqliteRow,
    ) -> Result<Option<Self>, sqlx::Error> {
        if column::<Option<i64>>(row, "block_position")?.is_none() {
            return Ok(None);
        }
        Self::from_row(row).map(Some)
    }

    /// `query` with the six values bound next, in the order
    /// the `block_*` columns are declared.
    pub(in crate::storage::sqlite) fn bind<'q>(&'q self, query: Bound<'q>) -> Bound<'q> {
        query
            .bind(self.position)
            .bind(self.height)
            .bind(self.hash.as_slice())
            .bind(self.parent_position)
            .bind(self.parent_hash.as_deref())
            .bind(self.timestamp)
    }

    /// The six values pushed next onto a row a statement builder is writing, in
    /// the order the `block_*` columns are declared.
    pub(in crate::storage::sqlite) fn push<'q>(
        &'q self,
        values: &mut Separated<'_, 'q, Sqlite, &'static str>,
    ) {
        values
            .push_bind(self.position)
            .push_bind(self.height)
            .push_bind(self.hash.as_slice())
            .push_bind(self.parent_position)
            .push_bind(self.parent_hash.as_deref())
            .push_bind(self.timestamp);
    }
}

impl TryFrom<&BlockRef> for Block {
    type Error = IndexError;

    fn try_from(block: &BlockRef) -> Result<Self, IndexError> {
        Ok(Self {
            position: stored(block.position.0)?,
            height: stored(block.height.0)?,
            hash: block.hash.0.clone(),
            parent_position: block
                .parent
                .as_ref()
                .map(|parent| stored(parent.position.0))
                .transpose()?,
            parent_hash: block.parent.as_ref().map(|parent| parent.hash.0.clone()),
            timestamp: block.timestamp.map(stored).transpose()?,
        })
    }
}

impl TryFrom<Block> for BlockRef {
    type Error = IndexError;

    fn try_from(block: Block) -> Result<Self, IndexError> {
        let parent = match (block.parent_position, block.parent_hash) {
            (Some(position), Some(hash)) => Some(BlockParent {
                position: BlockPosition(restored(position)?),
                hash: BlockHash(hash),
            }),
            (None, None) => None,
            _ => return Err(store("a stored block parent is half written")),
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
