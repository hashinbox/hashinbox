#![doc = include_str!("../README.md")]

mod address;
mod builder;
mod chain;
mod config;
mod erc20;
mod fee;
mod fee_estimator;
mod fee_rate;
mod key;
mod network;
mod rpc;
mod rpc_info;
#[cfg(feature = "testing")]
pub mod testing;
mod transaction;
mod wallet;

pub use address::Address;
pub use chain::Chain;
pub use config::{Config, Factory, Token};
/// The allowlisted ERC-20 contracts: the only tokens indexed and sent.
pub type Tokens = base::Tokens<alloy_primitives::Address>;
pub use fee::FeeLimits;
pub use key::KeyPair;
pub use network::{CHAIN_NAME, ETH_DECIMALS, ETH_NAME, ETH_TICKER, Network};
pub use rpc::Client;
pub use rpc_info::Info;
pub use transaction::Transaction;
pub use wallet::WalletFactory;
