#![doc = include_str!("../README.md")]

pub mod native;

mod builder;
mod config;
mod error;
mod identity;
mod indexer;
mod interpreter;
mod key;
pub mod sender;
mod source;
mod wallet;

pub use builder::TransferBuilder;
pub use config::{ChainHandle, Config, Limits, Store};
pub use indexer::{IndexerParams, indexer};
pub use interpreter::BlockInterpreter;
pub use native::{
    Address, AddressType, BTC, CHAIN_NAME, Error, ErrorKind, KeyPair, NETWORKS, Network, Satoshi,
    rpc::Client as RpcClient, transaction::FeeRate,
};
pub use source::Source;
pub use wallet::{WalletConfig, WalletFactory};
