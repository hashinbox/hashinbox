//! A block reference as the API publishes it.

use indexing::{IndexError, IndexErrorKind};
use serde::{Deserialize, Serialize};
use sqlx::{Row as _, sqlite::SqliteRow};

/// A block reference: where a transaction was included, or a checkpoint.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Block {
    /// Chain-native position (a block height or slot).
    pub(crate) position: u64,
    /// Block height.
    pub(crate) height: u64,
    /// Lowercase hex block hash.
    pub(crate) hash: String,
    /// The parent block, when known; always present.
    #[schema(required)]
    pub(crate) parent: Option<ParentBlock>,
}

/// The parent of a [`Block`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParentBlock {
    /// Chain-native position of the parent.
    position: u64,
    /// Lowercase hex parent hash.
    hash: String,
}

impl From<base::BlockRef> for Block {
    fn from(value: base::BlockRef) -> Self {
        Self::from(&value)
    }
}

impl From<&base::BlockRef> for Block {
    fn from(value: &base::BlockRef) -> Self {
        Self {
            position: value.position.0,
            height: value.height.0,
            hash: hex::encode(&value.hash.0),
            parent: value.parent.as_ref().map(|parent| ParentBlock {
                position: parent.position.0,
                hash: hex::encode(&parent.hash.0),
            }),
        }
    }
}

impl Block {
    /// The block indexing's six `block_*` columns of `row` hold.
    ///
    /// # Errors
    ///
    /// A terminal `Store` error for a missing column, a negative coordinate or
    /// a half-written parent.
    pub(crate) fn stored(row: &SqliteRow) -> Result<base::BlockRef, IndexError> {
        let column = |name: &str| -> Result<Option<i64>, IndexError> {
            row.try_get(name)
                .map_err(|_| stored("a stored block is unreadable"))
        };
        let coordinate = |value: Option<i64>| -> Result<Option<u64>, IndexError> {
            value
                .map(|value| u64::try_from(value).map_err(|_| stored("a stored block is negative")))
                .transpose()
        };
        let hash = |name: &str| -> Result<Option<Vec<u8>>, IndexError> {
            row.try_get(name)
                .map_err(|_| stored("a stored block is unreadable"))
        };
        let position = coordinate(column("block_position")?)?
            .ok_or_else(|| stored("a stored block has no position"))?;
        let height = coordinate(column("block_height")?)?
            .ok_or_else(|| stored("a stored block has no height"))?;
        let parent = match (
            coordinate(column("block_parent_position")?)?,
            hash("block_parent_hash")?,
        ) {
            (Some(position), Some(hash)) => Some(base::BlockParent {
                position: base::BlockPosition(position),
                hash: base::BlockHash(hash),
            }),
            (None, None) => None,
            _ => return Err(stored("a stored block parent is half written")),
        };
        Ok(base::BlockRef {
            position: base::BlockPosition(position),
            height: base::BlockHeight(height),
            hash: base::BlockHash(
                hash("block_hash")?.ok_or_else(|| stored("a stored block has no hash"))?,
            ),
            parent,
            timestamp: coordinate(column("block_timestamp")?)?,
        })
    }
}

/// Indexed data that contradicts the schema: retrying cannot fix it.
fn stored(message: &str) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::emits_required;

    #[test]
    fn a_block_requires_exactly_what_it_emits() {
        emits_required(
            "Block",
            Block {
                position: 7,
                height: 7,
                hash: "ab".repeat(32),
                parent: None,
            },
        );
    }
}
