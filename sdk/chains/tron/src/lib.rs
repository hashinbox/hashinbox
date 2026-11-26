//! Tron for hashinbox: reads blocks over a node's HTTP API as `base` blocks
//! with every leg of TRX and the allowlisted TRC-20 tokens and the TRX each
//! transaction burned, and builds (its protobuf encoded here), signs and
//! broadcasts TRX and token transfers.

mod address;
mod builder;
mod chain;
mod config;
mod fee;
mod key;
mod network;
mod rpc;
mod rpc_info;
#[cfg(feature = "testing")]
pub mod testing;
mod transaction;
mod transport;
mod trc20;
mod wallet;

pub use address::{Account, Address};
pub use chain::Chain;
pub use config::{Config, Factory, Token};
pub use fee::FeeLimits;
pub use key::KeyPair;
pub use network::{CHAIN_NAME, Network, TRX_DECIMALS, TRX_NAME, TRX_TICKER};
pub use rpc::Client;
pub use rpc_info::Info;
/// The allowlisted TRC-20 contracts: the only tokens indexed and sent.
pub type Tokens = base::Tokens<Account>;
pub use transaction::Transaction;
pub use transport::{REFUSAL, Rest};
pub use wallet::WalletFactory;
