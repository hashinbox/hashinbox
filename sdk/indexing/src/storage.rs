//! What a store must implement, and what it may.
//!
//! **Two ports are required.** [`watch::WatchStore`] persists what an index
//! observes and hands it back with its progress; [`commit::IndexStore`] holds the
//! network's checkpoint and takes the one write an index makes. Together they
//! are [`Repository`], and they are the whole of what a third party writing its
//! own index sink has to implement: registration and committing.
//!
//! **The rest are consumer capabilities**, and [`reads`] is all three of them.
//! They are what *this* workspace's consumers need — `apps/server`'s balance route,
//! Bitcoin's change selection, the rescan tool — and they are three traits so
//! that needing none of them is a legal store rather than a stubbed one.
//!
//! [`wallet::WalletRegistry`] is one more, on the same table as the watch list:
//! the application files a wallet's label and key beside its observation and
//! reads every wallet back, archived ones included. Scanning never reads it.
//!
//! Querying history is not among them: it is the repository's own
//! [`crate::ConfirmedHistory`] API, declared with the query it takes, so a sink
//! carries no pagination model to be a sink.
//!
//! A repository is not required to be [`Clone`], and none of these traits ask
//! for it. A sink that is neither cheap to clone nor clonable at all is held as
//! `Arc<MySink>`, which implements each port by forwarding to it.
//!
//! Everything above this module is the vocabulary a chain produces; everything
//! below it is a store, or what every store is asked and answers with: [`Balance`],
//! [`ConfirmedHistoryPage`] and the output requests, in the files beside this one.
//! Two stores are shipped and
//! either, both or neither may be built: [`sqlite`] on one embedded SQL file,
//! and [`memory`] on ordinary maps and no file at all. One store serves every
//! configured network: each operation names the network it is about, and rows
//! of one network are invisible to reads of another.

mod balance;
mod commit;
mod history;
mod join;
#[cfg(feature = "memory")]
pub mod memory;
mod observed;
mod outputs;
mod reads;
#[cfg(any(feature = "sqlite", feature = "memory"))]
mod spendable;
#[cfg(feature = "sqlite")]
pub mod sqlite;
mod wallet;
mod watch;

pub use balance::{Balance, Balances, Completeness, Fold, MovementEffect, PendingValue};
pub use commit::{IndexStore, IndexedBatch, Progress};
#[cfg(any(feature = "sqlite", feature = "memory"))]
pub(crate) use history::Probe;
pub use history::{ConfirmedHistory, ConfirmedHistoryPage, HistoryPosition, HistoryQuery, Page};
pub use join::{PendingJoin, Router};
pub use observed::{History, ObservedTransaction, TransactionPage, TransactionStatus};
pub use outputs::{OutputCursor, OutputPage, OutputRequest};
pub use reads::{BalanceQuery, BalanceStore, Inspection, OutputStore, PurgedRows, Purging};
pub use wallet::{Registration, Wallet, WalletRegistry};
pub use watch::{WatchProgress, WatchStore};

/// The whole of what an index requires of a store.
///
/// Registration and committing, and nothing else: no balance, no history, no
/// outputs, no purge. Blanket-implemented, so a type that implements both
/// halves *is* a repository without saying so, and one implementation serves
/// every network behind `Arc<dyn Repository>` without being [`Clone`].
pub trait Repository: WatchStore + IndexStore {}

impl<T: WatchStore + IndexStore + ?Sized> Repository for T {}
