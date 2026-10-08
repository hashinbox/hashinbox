//! **Contract.**
//!
//! What a store must implement, and what it may.
//!
//! **Two ports are required.** [`watch::WatchStore`] persists what an index
//! observes and hands it back with its progress; [`commit::IndexStore`] holds the
//! network's checkpoint, takes the one write an index makes and the one removal
//! (the rescan's purge). Together they
//! are [`Repository`], and they are the whole of what a third party writing its
//! own index sink has to implement: registration and committing.
//!
//! **The rest are narrow ports a consumer may need**: [`OutputStore`] (Bitcoin's
//! coin selection) and [`TransactionStore`] (Solana's absence proof). Balance
//! and history are not ports: the shipped stores answer them with inherent
//! methods, and a custom store brings its own.
//!
//! [`wallet::WalletRegistry`] is one more, on the same table as the watch list:
//! the application files a wallet's label and key beside its observation and
//! reads every wallet back, archived ones included. Scanning never reads it.
//!
//! A repository is not required to be [`Clone`], and none of these traits ask
//! for it. A sink that is neither cheap to clone nor clonable at all is held as
//! `Arc<MySink>`, which implements each port by forwarding to it.
//!
//! Two stores are shipped and either, both or neither may be built: [`sqlite`]
//! on one embedded SQL file, and [`memory`] on ordinary maps and no file at
//! all. One store serves every configured network: each operation names the
//! network it is about, and rows of one network are invisible to reads of
//! another.

mod balance;
mod commit;
mod effect;
mod history;
mod location;
#[cfg(feature = "memory")]
pub mod memory;
mod notifying;
mod outputs;
mod purge;
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub(crate) mod subject;
mod wallet;
mod watch;

pub use balance::{Balance, SettledBalance};
pub use commit::{IndexStore, IndexedBatch, Progress};
pub use effect::Net;
pub use history::{Entry, History, Page, Position, Standing};
pub use location::{Location, TransactionStore};
pub use notifying::{Change, Notifying};
#[cfg(any(feature = "sqlite", feature = "memory"))]
pub(crate) use outputs::Probe;
pub use outputs::{OutputCursor, OutputPage, OutputRequest, OutputStore};
pub use purge::PurgedRows;
pub use wallet::{Registration, Wallet, WalletRegistry};
pub use watch::{Inspection, WatchProgress, WatchStore};

/// The whole of what an index requires of a store.
///
/// Registration and committing, and nothing else: no balance, no history, no
/// outputs. Blanket-implemented, so a type that implements both
/// halves *is* a repository without saying so, and one implementation serves
/// every network behind `Arc<dyn Repository>` without being [`Clone`].
pub trait Repository: WatchStore + IndexStore {}

impl<T: WatchStore + IndexStore + ?Sized> Repository for T {}
