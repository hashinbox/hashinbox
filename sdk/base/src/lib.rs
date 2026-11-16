#![doc = include_str!("../README.md")]

mod address;
mod asset;
mod balance;
mod block;
mod cache;
mod chain;
mod config;
pub mod container;
mod fee;
mod indexer;
mod info;
mod inout;
mod network;
#[cfg(feature = "testing")]
pub mod testing;
/// Signed transactions, their identity, building, and broadcast.
pub mod transaction;
mod wallet;

pub use address::{Address, Addressed};
pub use asset::{Asset, AssetInfo, Assets};
pub use balance::{Balances, Holding};
pub use block::{
    Block, Blocks, Chained, Dated, Hash as BlockHash, Header as BlockHeader,
    Listener as BlockListener, NotServed, Notify as BlockNotify, Notifying, Parent as BlockParent,
    Probe as BlockProbe, Reward, Stamp as BlockStamp, Store as BlockStore, Tips,
};
pub use cache::Cache;
pub use chain::Chain;
pub use config::{AssetConfig, AssetKind, NetworkConfig, Service, ServiceFactory, Storage};
pub use container::{Container, Simple, Transport};
pub use decimal::{Decimal, Error as DecimalError};
pub use fee::{Estimator as FeeEstimator, Holdings, NoHoldings, Spend, Spends};
pub use indexer::{Birthday, DEFAULT_BATCH, Indexer, Next, Stats};
pub use info::{CachedInfo, InfoClient, NodeInfo};

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
    IndexState as WalletIndexState, IndexStore as WalletIndexStore, Store as WalletStore, Wallet,
};

use std::pin::Pin;

/// Boxed `Send` future used by object-safe async capability traits.
///
/// The SDK owns no async runtime and keeps its traits usable as `dyn`, so
/// trait methods return this instead of using `async fn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
