//! Bitcoin for hashinbox: reads blocks as `base` blocks with every leg, and
//! builds, signs and broadcasts BTC transfers from P2WPKH and P2TR wallets.
//! `native` is the protocol code alone, usable without the rest of the SDK.

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
#[cfg(feature = "testing")]
pub mod testing;
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
