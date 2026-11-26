//! The chain-neutral vocabulary every other SDK crate shares: networks,
//! addresses, assets, exact amounts, blocks and transaction identities, and
//! the small traits a chain implements to build and broadcast a transaction.

mod address;
mod asset;
mod balance;
mod block;
mod cache;
mod chain;
mod config;
pub mod container;
pub mod events;
mod fee;
mod index;
mod indexer;
mod info;
mod inout;
mod network;
#[cfg(feature = "testing")]
pub mod testing;
mod token;
/// Signed transactions, their identity, building, and broadcast.
pub mod transaction;
mod wallet;

pub use address::{Address, Addressed};
pub use asset::{Asset, AssetInfo, Assets};
pub use balance::{Balances, Holding};
pub use block::{
    Block, Blocks, Chained, Dated, Hash as BlockHash, Header as BlockHeader, Kept, Line,
    Listener as BlockListener, NotServed, Notify as BlockNotify, Notifying, Parent as BlockParent,
    Probe as BlockProbe, Reward, Stamp as BlockStamp, Store as BlockStore, Tips,
};
pub use cache::Cache;
pub use chain::Chain;
pub use config::{
    AssetConfig, AssetKind, NetworkConfig, Service, ServiceFactory, Storage, Tracking,
};
pub use container::{Container, Simple, Transport};
pub use decimal::{Decimal, Error as DecimalError};
pub use fee::{
    Estimator as FeeEstimator, Fee, Holdings, Level as FeeLevel, NoHoldings, Rate as FeeRate,
    RateProvider as FeeRateProvider, RateProviderCached as FeeRateProviderCached, Rule as FeeRule,
    Spend, Spends,
};
pub use index::{
    Birthday, Confirmed, Confirmer, IndexState, Indexed, IntoIndex, NoTransactionIndexes, Progress,
    Skipped, TransactionIndex, TransactionIndexStore, TransactionIndexTarget, TransactionIndexer,
    TransactionStatus, WalletIndex, WalletIndexStore, WalletIndexTarget, WalletIndexer,
};
pub use indexer::{DEFAULT_BATCH, Indexer, Indexes, Next, Stats};
pub use info::{CachedInfo, InfoClient, NodeInfo};

pub use inout::{Coin, Input, Inputs, Output, Store as InputStore};
pub use network::{Network, Networked};
pub use token::Tokens;
pub use transaction::{
    Broadcaster, Builder, Error as TransactionError, ErrorKind as TransactionErrorKind, Future,
    Id as TransactionId, Leg, Reason, Replace, Signer, State as TransactionState, Transacted,
    Transaction, Transactions, Transfer,
};
pub use wallet::{
    Error as WalletError, ErrorKind as WalletErrorKind, Factories as WalletFactories,
    Factory as WalletFactory, FactoryBroadcaster as WalletFactoryBroadcaster, Store as WalletStore,
    Wallet,
};

use std::pin::Pin;

/// Boxed `Send` future used by object-safe async capability traits.
///
/// The SDK owns no async runtime and keeps its traits usable as `dyn`, so
/// trait methods return this instead of using `async fn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
