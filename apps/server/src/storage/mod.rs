//! The one SQLite file as one pool ([`Storage`]) and every storage trait of
//! `base` and `tasks` on the models, one file per trait, plus the server's
//! own reads (`history`: balances and history pages; `endpoints`). `migrate` executes
//! the schemas; there are no migrations.
//!
//! Diesel is synchronous: every operation runs on Tokio's blocking pool with
//! one pooled connection, inside one transaction (`BEGIN IMMEDIATE` for
//! writes). A store only orchestrates; every statement is a model's.

mod blocks;
mod endpoints;
mod history;
mod indexes;
mod inputs;
mod tasks;
mod transactions;
mod wallets;

use std::{
    path::{self, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, ensure};
use diesel::{
    connection::SimpleConnection as _,
    prelude::*,
    r2d2::{ConnectionManager, CustomizeConnection, Pool},
};
use serde::Deserialize;
use tokio::sync::broadcast;

/// The `"storage"` configuration: `{"path": ...}`, defaulting to `payment.db`;
/// a relative path resolves against the working directory.
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

/// Every schema file, in the order `migrate` executes them.
const SCHEMAS: [&str; 3] = [
    include_str!("../../../../schema/sdk.sql"),
    include_str!("../../../../schema/server.sql"),
    include_str!("../../../../schema/queue.sql"),
];

impl Config {
    /// Opens the configured file and executes every schema on it (a new file).
    ///
    /// # Errors
    ///
    /// When the file does not open or a schema does not apply.
    pub(crate) async fn migrate(&self) -> anyhow::Result<()> {
        self.open()?.migrate().await
    }

    /// Opens the configured file as one pool; applies no schema.
    ///
    /// # Errors
    ///
    /// When the path does not resolve or the pool does not open.
    pub(crate) fn open(&self) -> anyhow::Result<Storage> {
        ensure!(
            !self.path.as_os_str().is_empty(),
            "storage path must not be empty"
        );
        let path = path::absolute(&self.path)
            .context("storage path is not resolvable against the working directory")?;
        Storage::open(&path)
    }
}

/// How long a statement waits for another connection's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Changes a slow queue subscriber may fall behind by before it lags.
const CHANGES: usize = 256;

/// The pragmas every connection runs when it opens.
#[derive(Debug)]
struct Pragmas;

impl CustomizeConnection<SqliteConnection, diesel::r2d2::Error> for Pragmas {
    fn on_acquire(&self, connection: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        connection
            .batch_execute(&format!(
                "PRAGMA busy_timeout = {}; PRAGMA journal_mode = WAL; \
                 PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON; PRAGMA temp_store = MEMORY;",
                BUSY_TIMEOUT.as_millis()
            ))
            .map_err(diesel::r2d2::Error::QueryError)
    }
}

/// The one SQLite file's bounded pool with every store on it. Clones share
/// the pool and the queue's change channel.
#[derive(Clone)]
pub(crate) struct Storage {
    pool: Pool<ConnectionManager<SqliteConnection>>,
    changes: broadcast::Sender<::tasks::Job>,
}

impl Storage {
    /// Opens or creates the SQLite file at `path` (absolute); applies no schema.
    ///
    /// # Errors
    ///
    /// When the path is relative or the pool does not open.
    pub(crate) fn open(path: &Path) -> anyhow::Result<Self> {
        ensure!(path.is_absolute(), "sqlite path must be absolute");
        let url = path.to_str().context("sqlite path is not UTF-8")?;
        let size = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 16));
        let pool = Pool::builder()
            .max_size(u32::try_from(size).unwrap_or(4))
            .min_idle(Some(0))
            .connection_customizer(Box::new(Pragmas))
            .build(ConnectionManager::new(url))
            .context("sqlite pool did not open")?;
        Ok(Self {
            pool,
            changes: broadcast::channel(CHANGES).0,
        })
    }

    /// Executes every schema file on this (new) file.
    ///
    /// # Errors
    ///
    /// When a schema does not apply.
    pub(crate) async fn migrate(&self) -> anyhow::Result<()> {
        self.write(|connection| {
            for schema in SCHEMAS {
                connection.batch_execute(schema)?;
            }
            Ok(())
        })
        .await
    }

    /// Runs `work` in one `BEGIN IMMEDIATE` transaction on a blocking thread;
    /// an error rolls it back. A commit error is not proof nothing landed.
    async fn write<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut SqliteConnection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = pool.get().context("sqlite could not lend a connection")?;
            (*connection).immediate_transaction(work)
        })
        .await
        .context("a storage write did not finish")?
    }

    /// Runs `work` in one read transaction (one snapshot) on a blocking thread.
    async fn read<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut SqliteConnection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = pool.get().context("sqlite could not lend a connection")?;
            (*connection).transaction(work)
        })
        .await
        .context("a storage read did not finish")?
    }
}
