//! Ethereum protocol code: RPC, block parsing, ERC-20 ABI, keys, amounts, and
//! transaction building, signing, and submission.
//!
//! Nothing here names an SDK contract (`indexing`, `wallets`) or implements
//! one; the adapter modules at the crate root do that. Use this module
//! directly for raw node access or protocol types.

mod address;
mod amount;
mod asset;
pub mod block;
pub mod erc20;
mod error;
mod key;
mod network;
pub mod rpc;
pub mod transaction;

pub use address::Address;
pub use amount::Wei;
pub use asset::{AssetKind, ETH};
pub use error::{Error, ErrorKind};
pub use key::KeyPair;
pub use network::{CHAIN_NAME, NETWORKS, Network};
pub use transaction::{SignedTransaction, TransactionId};
