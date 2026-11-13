#![doc = include_str!("../README.md")]

mod address;
mod builder;
mod chain;
mod config;
mod fee;
mod fee_cache;
mod key;
mod network;
mod rpc;
mod rpc_info;
mod transaction;
mod wallet;

pub use address::{Address, AddressType};
pub use chain::Chain;
pub use config::{Config, Factory};
pub use fee::FeeRate;
pub use key::KeyPair;
pub use network::{BTC_DECIMALS, BTC_NAME, BTC_TICKER, CHAIN_NAME, Network};
pub use rpc::Client;
pub use rpc_info::Info;
pub use transaction::Transaction;
pub use wallet::WalletFactory;
