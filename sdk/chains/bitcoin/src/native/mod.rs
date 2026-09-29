//! Bitcoin protocol code: Bitcoin Core JSON-RPC, block parsing with resolved
//! previous outputs, keys and addresses, amounts and fee rates, and
//! transaction building, signing, and submission.
//!
//! Nothing here names an SDK contract (`indexing`, `wallets`) or implements
//! one; the adapter modules at the crate root do that. Use this module
//! directly for raw node access or protocol types.

mod address;
mod amount;
mod asset;
pub mod block;
mod error;
mod json;
mod key;
mod network;
mod prevout;
pub mod rpc;
pub mod transaction;

pub use address::{Address, AddressType};
pub use amount::Satoshi;
pub use asset::{AssetKind, BTC};
pub use error::{Error, ErrorKind};
pub use key::KeyPair;
pub use network::{CHAIN_NAME, NETWORKS, Network};
pub use transaction::{SignedTransaction, TransactionId};
