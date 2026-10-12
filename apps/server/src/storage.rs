//! The one SQLite file, opened once as one pool. `migrate` executes the
//! schema (`schema/sdk.sql`, `schema/server.sql`, `schema/queue.sql`) on it;
//! there are no migrations and opening checks no version. Its indexing rows are handed to every chain as one
//! [`Repository`]; its `wallet` rows are `wallets::WalletRepository`'s; the
//! models (`crate::models`) use the pool.

use std::{path, path::PathBuf};

use serde::Deserialize;

use crate::config::AnyError;

/// The one repository every chain composes over: it implements the two ports an
/// index requires and every consumer capability this server's chains read.
pub(crate) type Repository = indexing::sqlite::Repository;

/// The `"storage"` configuration: `{"path": ...}`, defaulting to `payment.db`
/// in the working directory.
///
/// There is one store, so this names a file rather than choosing a backend. A
/// path may be written relative: this is the composition root, so it resolves
/// the path against the working directory before the backend, which only
/// accepts an absolute path, ever sees it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: PathBuf::from("payment.db"),
        }
    }
}

impl Config {
    pub(crate) fn validate(&self) -> Result<(), AnyError> {
        self.resolved_path().map(drop)
    }

    /// Opens the configured file and executes every schema on it: the SDK's,
    /// the server's (`crate::models::SCHEMA`) and the task queue's. For a new
    /// file; there are no migrations.
    pub(crate) async fn migrate(&self) -> Result<(), AnyError> {
        let store = self.open().await?;
        for schema in [
            include_str!("../../../schema/sdk.sql"),
            crate::models::SCHEMA,
            include_str!("../../../schema/queue.sql"),
        ] {
            sqlx::raw_sql(schema).execute(&store.pool).await?;
        }
        Ok(())
    }

    /// Opens the configured file as one pool; applies no schema.
    pub(crate) async fn open(&self) -> Result<Store, AnyError> {
        let path = self.resolved_path()?;
        let database = indexing::sqlite::Database::open(path).await?;
        Ok(Store {
            pool: database.pool().clone(),
            wallets: wallets::storage::sqlite::Repository::new(database.pool().clone()),
            indexing: indexing::sqlite::Repository::new(database),
        })
    }

    /// The database file as the backend requires it: absolute, resolved
    /// against the working directory. Validation runs it so a path that cannot
    /// be resolved fails at configuration time, before any chain connects.
    pub(crate) fn resolved_path(&self) -> Result<PathBuf, AnyError> {
        if self.path.as_os_str().is_empty() {
            return Err("storage path must not be empty".into());
        }
        path::absolute(&self.path).map_err(|error| {
            format!("storage path is not resolvable against the working directory: {error}").into()
        })
    }
}

/// The opened database file, shared by every network and every model, with
/// each owner's repository over it, built once.
pub(crate) struct Store {
    pool: sqlx::SqlitePool,
    indexing: Repository,
    wallets: wallets::storage::sqlite::Repository,
}

impl Store {
    /// Indexing's repository over this file. One value serves every network:
    /// each operation names the network it is about.
    pub(crate) fn indexing(&self) -> Repository {
        self.indexing.clone()
    }

    /// The wallets side's repository over this file: the `wallet` rows.
    pub(crate) fn wallets(&self) -> wallets::storage::sqlite::Repository {
        self.wallets.clone()
    }

    /// The file's one pool, for the models' stores.
    pub(crate) fn pool(&self) -> sqlx::SqlitePool {
        self.pool.clone()
    }
}
