//! Reorg-safe, chain-independent block synchronization contracts.

mod admission;
mod block;
mod composer;
mod error;
mod indexer;
mod observation;
mod observer;
mod output;
mod registry;
mod service;
mod source;
mod sync_policy;
mod synchronizer;
mod value;

pub use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, Decimal};
pub use block::{BlockAddition, BlockOutcome, BlockSelector, Blocks, InterpretedBlock};
pub use composer::Composer;
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use indexer::{Checkpoint, History, Indexer};
pub use observation::{
    CanonicalPage, CanonicalStatus, CanonicalTransaction, HistoryCursor, HistoryPosition,
    HistoryQuery, MovementId, MovementKind, NetworkFee, ObservationDraft, ObservationDraftStatus,
    ObservedTransaction, TransactionPage, TransactionStatus, Transactions, ValueMovement,
};
pub use observer::{BlockObservation, Observer};
pub use output::{
    IndexedOutput, OutputChanges, OutputCursor, OutputId, OutputKey, OutputPage, OutputRequest,
    Outputs,
};
pub use registry::{RegisteredAddress, Registry};
#[doc(hidden)]
pub use service::Service;
pub use service::{AddressFilter, FilterSource, SyncPhase, SyncStatus};
pub use source::{BlockInterpreter, BlockSource, IndexedBlock};
pub use sync_policy::SyncPolicy;
pub use synchronizer::SyncConfig;
pub use value::{AssetId, CanonicalAddress, ChainId, IndexScope, TransactionRef};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub use admission::{CommitPermit, PublicationPermit, ScopeAdmission, SyncPlan};

#[cfg(test)]
mod synchronizer_test {
    use crate::{ChainId, IndexErrorKind, IndexScope, SyncConfig, SyncPolicy};

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("test".into()),
            network: "mainnet".into(),
        }
    }

    #[test]
    fn configuration_requires_bounded_rollback_and_confirmation() {
        assert!(SyncConfig::new(scope(), SyncPolicy::new(1, 1, 100)).is_ok());
        assert_eq!(
            SyncConfig::new(scope(), SyncPolicy::new(1, 0, 100))
                .unwrap_err()
                .kind,
            IndexErrorKind::InvalidRequest
        );
    }
}
