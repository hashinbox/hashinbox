//! What one network's indexer did since the process started, as the
//! networks and health routes show it.

use serde::{Deserialize, Serialize};

/// One network's indexing health, from memory.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct Indexing {
    /// Blocks kept.
    pub(crate) blocks: u64,
    /// Native position of the last block kept; always present.
    #[schema(required)]
    pub(crate) last: Option<u64>,
    /// The chain's produced tip at the last step; always present.
    #[schema(required)]
    pub(crate) tip: Option<u64>,
    /// Kept blocks archived because the chain took them back.
    pub(crate) reorgs: u64,
    /// Watches added.
    pub(crate) watched: u64,
    /// Failed steps.
    pub(crate) failures: u64,
    /// The last failure, cleared by the next kept block; always present.
    #[schema(required)]
    pub(crate) error: Option<String>,
}

impl From<base::Stats> for Indexing {
    fn from(stats: base::Stats) -> Self {
        Self {
            blocks: stats.blocks,
            last: stats.last.map(|header| header.position),
            tip: stats.tip,
            reorgs: stats.reorgs,
            watched: stats.watched,
            failures: stats.failures,
            error: stats.error,
        }
    }
}
