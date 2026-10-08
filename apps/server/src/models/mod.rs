//! The nouns the server uses across its endpoints and its storage: each one's
//! validated type, its wire form where it has one, and, for the durable ones,
//! where it is kept. One file per noun.
//!
//! A model names no HTTP framework and no chain. A wallet is the SDK's
//! `wallet` row, read through `wallets::WalletRepository`, with the assets the
//! index observes of it; balances and history are the repository's own reads
//! (`indexing::sqlite::Repository`); the server's own table (`rpc_endpoints`)
//! is in `schema/server.sql` at the repository root, in the same SQLite file
//! as the SDK's, so a network purge, which is indexing's, never touches it. The schema is applied
//! when storage opens ([`SCHEMA`]); a model
//! assumes it.

mod asset;
mod block;
mod endpoint;
mod network;
mod signers;
mod wallet;

pub(crate) use asset::Asset;
pub(crate) use block::Block;
pub(crate) use endpoint::{Endpoint, NetworkEndpoints, RpcEndpoints};
pub(crate) use network::Network;
pub(crate) use signers::Signers;
pub(crate) use wallet::{Label, Wallet};

/// The server's own table (`schema/server.sql`), created beside the SDK's
/// when storage creates the file (`indexing::sqlite::Database::open_with`).
/// There are no migrations: the file's identity covers this text, so a schema
/// change needs a fresh file.
pub(crate) const SCHEMA: &str = include_str!("../../../../schema/server.sql");
