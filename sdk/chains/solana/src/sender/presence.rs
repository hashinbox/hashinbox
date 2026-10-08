//! [`Presence`]: whether a submitted transaction is indexed, the half of the
//! absence proof the index owns.

use std::sync::Arc;

use base::{BlockRef, BoxFuture, TransactionRef};
use indexing::{IndexError, Pending, TransactionStore};

/// Whether one transaction is indexed, and the checkpoint the answer was read
/// at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing {
    /// The confirmed line of the read; `None` before the network's first commit.
    pub checkpoint: Option<BlockRef>,
    /// Whether the transaction is confirmed or in a block above the line.
    pub present: bool,
}

/// Where recovery learns whether an unknown submission landed. Absence is
/// proven only when `present` is false at a checkpoint at or past the
/// transaction's last valid height; recovery checks both.
pub trait Presence: Send + Sync {
    /// Whether `transaction` is indexed.
    ///
    /// # Errors
    ///
    /// Whatever the source cannot read; recovery retries.
    fn landed<'a>(
        &'a self,
        transaction: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Landing, IndexError>>;
}

/// Store-backed presence: the store's confirmed transactions, with the
/// pending blocks the index holds above the store's line folded in.
pub struct IndexPresence {
    store: Arc<dyn TransactionStore>,
    pending: Arc<dyn Pending>,
}

impl IndexPresence {
    /// `pending` must be the index that commits into `store` for the network
    /// this presence answers for.
    #[must_use]
    pub fn new(store: Arc<dyn TransactionStore>, pending: Arc<dyn Pending>) -> Self {
        Self { store, pending }
    }
}

impl Presence for IndexPresence {
    /// Samples the pending blocks **before** reading the store and narrows the
    /// sample by the store's own checkpoint, so a commit landing in between
    /// is seen once.
    fn landed<'a>(
        &'a self,
        transaction: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Landing, IndexError>> {
        Box::pin(async move {
            let sample = self.pending.sampled()?;
            let located = self.store.locate(transaction).await?;
            let pending = sample.above(&located).iter().any(|held| {
                held.block
                    .transactions()
                    .iter()
                    .any(|entry| &entry.id == transaction)
            });
            Ok(Landing {
                present: located.block.is_some() || pending,
                checkpoint: located.checkpoint,
            })
        })
    }
}
