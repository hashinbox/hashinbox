//! The one SQLite file as one bounded pool ([`Db`]): the pragmas every
//! connection runs, the read and write transactions, and executing schemas.
//! It knows no model, no schema and no queue; anything may use it.
//!
//! Diesel is synchronous: every operation runs on Tokio's blocking pool with
//! one pooled connection, inside one transaction (`BEGIN IMMEDIATE` for
//! writes). There are no migrations: `migrate` executes the given schemas.

use std::{path::Path, time::Duration};

use anyhow::{Context as _, ensure};
use diesel::{
    connection::SimpleConnection as _,
    prelude::*,
    r2d2::{ConnectionManager, CustomizeConnection, Pool},
};

/// How long a statement waits for another connection's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The one SQLite file's bounded pool. Clones share the pool.
#[derive(Clone)]
pub(crate) struct Db {
    pool: Pool<ConnectionManager<SqliteConnection>>,
}

impl Db {
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
        Ok(Self { pool })
    }

    /// Executes `schemas`, in order, in one write on this (new) file.
    ///
    /// # Errors
    ///
    /// When a schema does not apply.
    pub(crate) async fn migrate(&self, schemas: &[&'static str]) -> anyhow::Result<()> {
        let schemas = schemas.to_vec();
        self.write(move |connection| {
            for schema in schemas {
                connection.batch_execute(schema)?;
            }
            Ok(())
        })
        .await
    }

    /// Runs `work` in one `BEGIN IMMEDIATE` transaction on a blocking thread;
    /// an error rolls it back. A commit error is not proof nothing landed.
    ///
    /// # Errors
    ///
    /// When no connection is lent, or `work` or the commit fails.
    pub(crate) async fn write<T: Send + 'static>(
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
    ///
    /// # Errors
    ///
    /// When no connection is lent, or `work` fails.
    pub(crate) async fn read<T: Send + 'static>(
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
