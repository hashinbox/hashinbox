//! The nouns the server uses across its endpoints and its storage: each one's
//! validated type, its wire form where it has one, and, for the durable ones,
//! where it is kept. One file per noun.
//!
//! A model names no HTTP framework and no chain. A wallet is indexing's
//! `wallet` row, read through `indexing::WalletRegistry`; balances and history
//! are the repository's own reads (`indexing::sqlite::Repository`); the
//! server's own table (`rpc_endpoints`) is in `schema.sql` beside this file, in the same
//! SQLite file as indexing's, so a network purge, which is indexing's, never
//! touches it. The schema is applied when storage opens ([`SCHEMA`]); a model
//! assumes it.

mod asset;
mod block;
mod endpoint;
mod network;
mod signers;
mod wallet;

pub(crate) use asset::Asset;
pub(crate) use block::Block;
pub(crate) use endpoint::RpcEndpoints;
pub(crate) use network::Network;
pub(crate) use signers::Signers;
pub(crate) use wallet::{Label, Wallet};

/// The server's own table (`schema.sql` beside this file), created beside
/// indexing's when storage creates the file
/// (`indexing::sqlite::Database::open_with`). There are no migrations: the
/// file's identity covers this text, so a schema change needs a fresh file.
pub(crate) const SCHEMA: &str = include_str!("schema.sql");

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    /// One database file in a temporary directory, not yet created.
    pub(crate) struct File {
        _directory: TempDir,
        pub(crate) path: PathBuf,
    }

    impl File {
        pub(crate) fn new() -> Self {
            let directory = tempfile::tempdir().expect("a temporary directory");
            Self {
                path: directory.path().join("server.db"),
                _directory: directory,
            }
        }

        /// The file's pool, opened as storage opens it; creates it.
        pub(crate) async fn pool(&self) -> sqlx::SqlitePool {
            self.database().await.pool().clone()
        }

        /// The file opened as storage opens it; creates it.
        pub(crate) async fn database(&self) -> indexing::sqlite::Database {
            indexing::sqlite::Database::open_with(&self.path, super::SCHEMA)
                .await
                .expect("the database")
        }
    }
}
