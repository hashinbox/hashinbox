//! The one SQLite file: opened once as one pool, its indexing schema handed
//! to every chain as one [`Repository`] — which also holds every wallet
//! (`indexing::WalletRegistry`) — and the server's own table
//! (`models/schema.sql`) created beside it, in the same file and under the
//! same identity, for the models (`crate::models`).

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

    /// Opens the configured file as one pool: an empty file is given
    /// indexing's schema and the server's (`crate::models::SCHEMA`), a file
    /// holding exactly those is opened as it is, and any other is refused.
    pub(crate) async fn open(&self) -> Result<Store, AnyError> {
        let path = self.resolved_path()?;
        let database = indexing::sqlite::Database::open_with(path, crate::models::SCHEMA).await?;
        Ok(Store {
            pool: database.pool().clone(),
            repository: indexing::sqlite::Repository::new(database),
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

/// The opened database file, shared by every network and every model.
pub(crate) struct Store {
    pool: sqlx::SqlitePool,
    repository: Repository,
}

impl Store {
    /// The repository over this file. One value serves every network: each
    /// operation names the network it is about.
    pub(crate) fn repository(&self) -> Repository {
        self.repository.clone()
    }

    /// The file's one pool, for the models' stores.
    pub(crate) fn pool(&self) -> sqlx::SqlitePool {
        self.pool.clone()
    }
}
