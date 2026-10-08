#![doc = include_str!("../README.md")]

mod block;
mod error;
mod gate;
mod indexers;
mod pending;
mod read;
mod run;
mod storage;
// Private, and re-exported below exactly as `engine::supervisor` was: the
// module is an implementation detail, its six types are the API.
#[cfg(feature = "driver")]
mod driver;
mod sync;
#[cfg(feature = "testing")]
pub mod testing;
mod watches;

pub use base::{
    Address, AssetId, Block, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
    BoxFuture, ChainName, Decimal, Holder, Identified, MovementKind, Network, Networked, Subject,
    TransactionRef, Transactions,
};

// What a chain implements, and the vocabulary it produces.
pub use block::{BlockCredit, BlockWithOutputs};
pub use block::{BlockInterpreter, BlockSource, ChainTips, FeeSettlement, NativeBlock};
pub use block::{IndexedOutput, OutputChanges, OutputId, OutputKey};
pub use block::{MovementId, NetworkFee, Status, Transaction, ValueMovement};
pub use block::{require_chain, select_addresses};
// What a store implements: the required ports, the capabilities, their rows.
pub use storage::{
    Change, IndexStore, IndexedBatch, Inspection, Location, Notifying, OutputStore, Progress,
    PurgedRows, Registration, Repository, TransactionStore, Wallet, WalletRegistry, WatchProgress,
    WatchStore,
};
// What an application calls: the index, the two facts only it holds, and the
// hook. The queries and their vocabulary are the repository's, from `storage`.
pub use error::{IndexError, IndexErrorKind, SourceError};
pub use indexers::{BlockCommit, Listener};
pub use indexers::{Checkpoint, Indexer, Indexing};
pub use pending::{Checkpointed, Pending, Sample};
pub use storage::{Balance, Entry, History, Page, Position, SettledBalance, Standing};
pub use storage::{Net, OutputCursor, OutputPage, OutputRequest};
pub use sync::{Backlog, Outcome, Phase};
pub use sync::{Condition, Health, SyncHealth};
pub use watches::{Birthday, ForwardRange, Watch, Watching};
// What the pass owns: the owners, the router, the policy and the cadence.
pub use indexers::{Indexers, Route};
pub use sync::Schedule;
pub use sync::{Budget, Config};

pub(crate) use gate::{Gate, Plan};

/// Every store port on one embedded SQLite file, for every network in it.
#[cfg(feature = "sqlite")]
pub use storage::sqlite;

/// Every store port in this process's memory, for every network in it.
#[cfg(feature = "memory")]
pub use storage::memory;

#[cfg(feature = "driver")]
pub use driver::{Control, Driver, Monitor, Readiness, Report, RescanError};
