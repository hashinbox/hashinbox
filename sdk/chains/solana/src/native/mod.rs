//! Solana protocol code: JSON-RPC, block parsing with System- and SPL
//! Token-transfer decoding, keys and addresses, lamports, and transfer
//! building, signing, and submission.
//!
//! Nothing here names an SDK contract (`indexing`, `wallets`) or implements
//! one; the adapter modules at the crate root do that. Use this module
//! directly for raw node access or protocol types.

mod account;
mod address;
mod amount;
mod asset;
pub mod block;
mod error;
mod key;
mod memo;
mod network;
pub mod rpc;
pub mod spl;
pub mod transaction;

pub use account::{Account, Bracket};
pub use address::Address;
pub use amount::Lamport;
pub use asset::{AssetKind, SOL};
pub use error::{Error, ErrorKind};
pub use key::{KeyPair, Seed};
pub use memo::Memo;
pub use network::{CHAIN_NAME, GenesisHash, NETWORKS, Network};
pub use transaction::{SignedTransaction, TransactionId};
