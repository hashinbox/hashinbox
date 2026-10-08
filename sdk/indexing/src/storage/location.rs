//! [`TransactionStore`]: whether the store holds one transaction, at the
//! checkpoint it answered from.

use crate::{BlockRef, BoxFuture, Checkpointed, IndexError, TransactionRef};

/// Where one transaction stands in a store, read in one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The network's checkpoint the answer was read at; `None` before its
    /// first commit.
    pub checkpoint: Option<BlockRef>,
    /// The block that includes the transaction, when the store holds it.
    pub block: Option<BlockRef>,
}

/// The answer and its checkpoint are one snapshot, so the answer is what a
/// pending sample is narrowed by.
impl Checkpointed for Location {
    fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
    }
}

/// Looks one confirmed transaction up by identity.
///
/// A store port, not part of [`crate::Repository`]. Its SDK consumer is
/// Solana's absence proof (`chain_solana::sender::IndexPresence`), which folds the
/// pending blocks over the answer.
pub trait TransactionStore: Send + Sync {
    /// Whether the store holds `transaction`, and the checkpoint of the read.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::Store`] when the read fails.
    fn locate<'a>(
        &'a self,
        transaction: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Location, IndexError>>;
}

/// The forwarding impl, so a store that cannot be cloned is used as
/// `Arc<MySink>`.
impl<T: TransactionStore + ?Sized> TransactionStore for std::sync::Arc<T> {
    fn locate<'a>(
        &'a self,
        transaction: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Location, IndexError>> {
        (**self).locate(transaction)
    }
}
