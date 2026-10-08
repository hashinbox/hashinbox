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

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockPosition, ChainName, Network};
    use futures_executor::block_on;
    use indexing::{BlockWithOutputs, Location, Sample, Status, Transaction};

    use super::*;

    fn network() -> Network {
        Network::new(ChainName::new("solana"), "localnet")
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: None,
            timestamp: None,
        }
    }

    fn sent() -> TransactionRef {
        TransactionRef::new(network(), "sent")
    }

    /// A store holding `block` for the transaction, at checkpoint `line`.
    struct Store(Location);

    impl TransactionStore for Store {
        fn locate<'a>(
            &'a self,
            _transaction: &'a TransactionRef,
        ) -> BoxFuture<'a, Result<Location, IndexError>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    /// An index holding `blocks` above its line.
    struct Held(Vec<BlockWithOutputs>);

    impl Pending for Held {
        fn sampled(&self) -> Result<Sample, IndexError> {
            Ok(Sample::new(self.0.clone(), 0))
        }
    }

    /// Block `height` carrying the sent transaction.
    fn carrying(height: u64) -> BlockWithOutputs {
        let transaction = Transaction {
            id: sent(),
            movements: Vec::new(),
            fee: None,
            order: 0,
            status: Status::Included {
                block: block(height),
            },
        };
        BlockWithOutputs::new(&network(), block(height), vec![transaction]).expect("a block")
    }

    fn landed(located: Location, held: Vec<BlockWithOutputs>) -> Landing {
        let presence = IndexPresence::new(Arc::new(Store(located)), Arc::new(Held(held)));
        block_on(presence.landed(&sent())).expect("an answer")
    }

    #[test]
    fn a_confirmed_or_pending_transaction_is_present_and_otherwise_absent_at_the_line() {
        let stored = Location {
            checkpoint: Some(block(5)),
            block: Some(block(4)),
        };
        assert!(landed(stored, Vec::new()).present, "confirmed");

        let above = Location {
            checkpoint: Some(block(5)),
            block: None,
        };
        assert!(landed(above.clone(), vec![carrying(6)]).present, "pending");

        // A block the store already committed is narrowed out of the sample:
        // it is not counted as pending, and the store says it is not there.
        let absent = landed(above, vec![carrying(5)]);
        assert!(!absent.present);
        assert_eq!(
            absent.checkpoint,
            Some(block(5)),
            "proven at the store's line"
        );
    }
}
