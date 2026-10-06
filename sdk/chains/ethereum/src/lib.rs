#![doc = include_str!("../README.md")]

pub mod native;

mod builder;
mod config;
mod error;
mod identity;
mod indexer;
mod interpreter;
mod key;
mod sender;
mod source;
mod wallet;

pub use builder::TransferBuilder;
pub use config::{ChainHandle, Config, Limits, Store};
pub use indexer::{IndexerParams, indexer};
pub use interpreter::BlockInterpreter;
pub use native::{
    Address, AssetKind, CHAIN_NAME, ETH, Error, ErrorKind, KeyPair, NETWORKS, Network, Wei,
    rpc::Client as RpcClient, transaction::FeeLimits,
};
pub use source::Source;
pub use wallet::{WalletConfig, WalletFactory};
