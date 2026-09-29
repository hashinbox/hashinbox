#![doc = include_str!("../README.md")]

mod admission;
mod backfill;
mod balance;
mod block;
mod catchup;
mod error;
mod fetch;
mod health;
mod history;
mod holder;
mod index;
mod indexer;
mod listeners;
mod network;
mod observation;
mod output;
mod overlay;
mod pass;
mod policy;
mod reads;
mod scan;
mod schedule;
mod service;
mod source;
mod storage;
// Private, and re-exported below exactly as `engine::supervisor` was: the
// module is an implementation detail, its six types are the API.
#[cfg(feature = "supervisor")]
mod supervisor;
mod sync;
mod watch;
mod watches;

pub use base::{
    Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, BoxFuture,
    ChainName, Decimal, MovementKind, Network, Networked, TransactionRef,
};

// What a chain implements, and the vocabulary it produces.
pub use block::{BlockCredit, CanonicalBlock, InterpretedBlock};
pub use network::{require_chain, select_addresses};
pub use observation::{
    CanonicalStatus, CanonicalTransaction, MovementId, NetworkFee, ObservationDraft,
    ObservationDraftStatus, ObservedTransaction, TransactionStatus, ValueMovement,
};
pub use output::{IndexedOutput, OutputChanges, OutputId, OutputKey};
pub use source::{BlockInterpreter, BlockSource, ChainTip, IndexedBlock};
// What a store implements: the required ports, the capabilities, their rows.
pub use storage::{
    BalanceQuery, BalanceStore, IndexStore, IndexedBatch, OutputStore, Progress, PurgedRows,
    Purging, Repository, WatchProgress, WatchStore,
};
// What an application calls: the reads, their vocabulary, and the hook.
pub use backfill::Backfilling;
pub use balance::{Balance, Balances, Completeness, Effect, FeeSettlement, Ledger};
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use health::{NetworkHealth, NetworkState, SyncHealth};
pub use history::{
    CanonicalPage, ConfirmedHistory, History, HistoryPosition, HistoryQuery, Page, PendingValue,
    TransactionPage,
};
pub use holder::{Holder, Subject};
pub use indexer::{Checkpoint, Indexing};
pub use listeners::{BlockCommit, Listener};
pub use output::{OutputCursor, OutputPage, OutputRequest};
pub use reads::{ObservedBalance, ObservedHistory, ObservedOutputs};
pub use sync::{BackfillStatus, SyncPhase, SyncStatus};
pub use watch::{AddressFilter, Start, Watch, Watching};
// What the pass owns: the owners, the router, the policy and the cadence.
pub use index::{Assembly, Index, SyncUnit};
pub use policy::{SyncConfig, SyncPolicy};
pub use schedule::{Schedule, Schedules};
pub use service::Service;

pub(crate) use admission::{NetworkAdmission, SyncPlan};

/// Every store port on one embedded SQLite file, for every network in it.
#[cfg(feature = "sqlite")]
pub use storage::sqlite;

/// Every store port in this process's memory, for every network in it.
#[cfg(feature = "memory")]
pub use storage::memory;

#[cfg(feature = "supervisor")]
pub use supervisor::{NetworkEvent, Readiness, Supervisor, SyncControl, SyncMonitor, SyncReport};
