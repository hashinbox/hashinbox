#![doc = include_str!("../README.md")]

mod address;
mod asset;
mod block;
mod cache;
mod chain;
mod config;
mod fee;
mod indexer;
mod inout;
mod network;
/// Signed transactions, their identity, building, and broadcast.
pub mod transaction;
mod wallet;

pub use address::{Address, Addressed};
pub use asset::{Asset, AssetInfo, Assets};
pub use block::{
    Block, Blocks, Chained, Hash as BlockHash, Header as BlockHeader, Listener as BlockListener,
    Notified, Notify as BlockNotify, Notifying, Parent as BlockParent, Reward, Store as BlockStore,
    Timer, Tips,
};
pub use cache::Cache;
pub use chain::Chain;
pub use config::{Config, Service, Storage};
pub use decimal::{Decimal, Error as DecimalError};
pub use fee::Estimator as FeeEstimator;
pub use indexer::{Birthday, DEFAULT_BATCH, Handle, Handles, Indexer, Indexers, Next, Stats};

pub use inout::{Coin, Input, Inputs, Output, Outputs, Store as InputStore};
pub use network::{Network, Networked};
pub use transaction::{
    Broadcaster, Builder, BuilderExt, Error as TransactionError, ErrorKind as TransactionErrorKind,
    Future, Id as TransactionId, Leg, Reason, Signer, State as TransactionState, Transacted,
    Transaction, Transactions, Transfer,
};
pub use wallet::{
    Error as WalletError, ErrorKind as WalletErrorKind, Factories as WalletFactories,
    Factory as WalletFactory, FactoryBroadcaster as WalletFactoryBroadcaster, Index as WalletIndex,
    IndexStore as WalletIndexStore, Store as WalletStore, Wallet,
};

use std::pin::Pin;

/// Boxed `Send` future used by object-safe async capability traits.
///
/// The SDK owns no async runtime and keeps its traits usable as `dyn`, so
/// trait methods return this instead of using `async fn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
