#![doc = include_str!("../README.md")]

mod address;
mod asset;
mod block;
mod chain;
mod network;
/// Signed transactions, their identity, building, and broadcast.
pub mod transaction;
mod wallet;

pub use address::Address;
pub use asset::{Asset, AssetInfo, Assets};
pub use block::{Block, Hash as BlockHash, Header as BlockHeader, Parent as BlockParent};
pub use chain::Chain;
pub use decimal::{Decimal, Error as DecimalError};

pub use network::Network;
pub use transaction::{
    Broadcaster, Builder, Error as TransactionError, ErrorKind as TransactionErrorKind, Future,
    Id as TransactionId, Reason,
};
pub use wallet::{
    Error as WalletError, ErrorKind as WalletErrorKind, Factories as WalletFactories,
    Factory as WalletFactory, FactoryBroadcaster as WalletFactoryBroadcaster, Wallet,
};

use std::pin::Pin;

/// Boxed `Send` future used by object-safe async capability traits.
///
/// The SDK owns no async runtime and keeps its traits usable as `dyn`, so
/// trait methods return this instead of using `async fn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
