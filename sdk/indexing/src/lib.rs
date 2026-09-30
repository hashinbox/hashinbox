#![doc = include_str!("../README.md")]

mod admission;
mod backfill;
mod block;
mod catchup;
mod error;
mod fetch;
mod health;
mod holder;
mod index;
mod indexer;
mod listeners;
mod network;
mod observation;
mod output;
mod pass;
mod pending;
mod policy;
mod scan;
mod schedule;
mod source;
mod storage;
// Private, and re-exported below exactly as `engine::supervisor` was: the
// module is an implementation detail, its six types are the API.
#[cfg(feature = "supervisor")]
mod supervisor;
mod sync;
mod watch;
mod watch_set;
mod watches;

pub use base::{
    Address, AssetId, Block, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
    BoxFuture, ChainName, Decimal, Identified, MovementKind, Network, Networked, TransactionRef,
    Transactions,
};

// What a chain implements, and the vocabulary it produces.
pub use block::{BlockCredit, CanonicalBlock, Facts, InterpretedBlock};
pub use network::{require_chain, select_addresses};
pub use observation::{
    CanonicalStatus, CanonicalTransaction, MovementId, NetworkFee, ObservationDraft,
    ObservationDraftStatus, ValueMovement,
};
pub use output::{IndexedOutput, OutputChanges, OutputId, OutputKey};
pub use source::{BlockInterpreter, BlockSource, ChainTips, FeeSettlement, NativeBlock};
// What a store implements: the required ports, the capabilities, their rows.
pub use storage::{
    Activity, ActivityKind, ActivityPage, ActivityPosition, ActivityQuery, ActivityTransaction,
    BalanceEvidence, BalanceQuery, BalanceStore, Direction, IndexStore, IndexedBatch, Inspection,
    OutputStore, Progress, PurgedRows, Purging, Repository, WatchProgress, WatchStore,
};
// What an application calls: the index, the two facts only it holds, and the
// hook. The queries and their vocabulary are the repository's, from `storage`.
pub use backfill::Backfilling;
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use health::{NetworkHealth, NetworkState, SyncHealth};
pub use holder::{Holder, Subject};
pub use indexer::{Checkpoint, Indexer, Indexing};
pub use listeners::{BlockCommit, Listener};
pub use pending::{Checkpointed, Pending, PendingSample};
pub use storage::{
    Balance, CanonicalPage, Completeness, ConfirmedHistory, Fold, HistoryPosition, HistoryQuery,
    MovementEffect, OutputCursor, OutputPage, OutputRequest, Page, PendingValue,
};
pub use sync::{BackfillStatus, SyncPhase, SyncStatus};
pub use watch::{Birthday, ForwardRange, Watch, Watching};
// What the pass owns: the owners, the router, the policy and the cadence.
pub use index::{Assembly, Index, SyncUnit};
pub use policy::{SyncConfig, SyncPolicy};
pub use schedule::{Schedule, Schedules};

pub(crate) use admission::{CommitGate, SyncPlan};

/// Every store port on one embedded SQLite file, for every network in it.
#[cfg(feature = "sqlite")]
pub use storage::sqlite;

/// Every store port in this process's memory, for every network in it.
#[cfg(feature = "memory")]
pub use storage::memory;

#[cfg(feature = "supervisor")]
pub use supervisor::{NetworkEvent, Readiness, Supervisor, SyncControl, SyncMonitor, SyncReport};
