#![doc = include_str!("../README.md")]

mod address;
mod builder;
mod chain;
mod config;
mod fee;
mod fee_estimator;
mod fee_rate;
mod key;
mod network;
mod rpc;
mod rpc_info;
mod spl;
#[cfg(feature = "testing")]
pub mod testing;
mod transaction;
mod wallet;

pub use address::Address;
pub use chain::{Chain, READ_DEPTH};
pub use config::{Config, Factory, Token};
pub use key::KeyPair;
pub use network::{CHAIN_NAME, GenesisHash, Network, SOL_DECIMALS, SOL_NAME, SOL_TICKER};
pub use rpc::Client;
pub use rpc_info::Info;
/// The allowlisted SPL mints and their decimals: the only tokens indexed
/// and sent.
pub type Tokens = base::Tokens<solana_address::Address>;
pub use transaction::Transaction;
pub use wallet::WalletFactory;
