//! A block reference as the API publishes it.

use serde::{Deserialize, Serialize};

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
