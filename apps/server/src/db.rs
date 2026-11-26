//! The one SQLite file as one bounded pool ([`Db`]): the pragmas every
//! connection runs, the read and write transactions, executing schemas, and
//! keyset paging ([`Page`], [`Keyed`], [`Paged`]).
//! It knows no model, no schema and no queue; anything may use it.
//!
//! Diesel is synchronous: every operation runs on Tokio's blocking pool with
//! one pooled connection, inside one transaction (`BEGIN IMMEDIATE` for
//! writes). There are no migrations: a new file gets the schema once
//! ([`Db::open_or_create`]).

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
            .map_err(diesel::r2d2::Error::QueryError)?;
        register(connection).map_err(diesel::r2d2::Error::QueryError)
    }
}

diesel::define_sql_function! {
    /// Exact sum of decimal texts; NULL when one does not decode.
    #[aggregate]
    #[sql_name = "SUM_DECIMAL"]
    fn sum_decimal(amount: diesel::sql_types::Text) -> diesel::sql_types::Nullable<diesel::sql_types::Text>;
}

diesel::define_sql_function! {
    /// Whether two job topics may not run at once ([`tasks::Topic::conflicts`]);
    /// false when either is not a topic.
    #[sql_name = "CONFLICTS"]
    fn conflicts(a: diesel::sql_types::Text, b: diesel::sql_types::Text) -> diesel::sql_types::Bool;
}

diesel::define_sql_function! {
    /// Whether job `id` (`status`, `run_at`, `attempts`) blocks job `job` at
    /// `now`, their topics conflicting ([`tasks::blocks`]).
    #[sql_name = "BLOCKS"]
    fn blocks(
        id: diesel::sql_types::BigInt,
        status: diesel::sql_types::Integer,
        run_at: diesel::sql_types::BigInt,
        attempts: diesel::sql_types::Integer,
        job: diesel::sql_types::BigInt,
        now: diesel::sql_types::BigInt
    ) -> diesel::sql_types::Bool;
}

/// `SUM_DECIMAL`'s running sum: `None` once a value does not decode.
struct SumDecimal(Option<base::Decimal>);

impl Default for SumDecimal {
    fn default() -> Self {
        Self(Some(base::Decimal::zero()))
    }
}

impl diesel::sqlite::SqliteAggregateFunction<String> for SumDecimal {
    type Output = Option<String>;

    fn step(&mut self, amount: String) {
        self.0 = self
            .0
            .take()
            .zip(amount.parse::<base::Decimal>().ok())
            .map(|(sum, amount)| sum + amount);
    }

    fn finalize(sum: Option<Self>) -> Option<String> {
        sum.unwrap_or_default().0.map(|sum| sum.to_string())
    }
}

/// Registers the SQL functions every connection has (`SUM_DECIMAL`,
/// `CONFLICTS`, `BLOCKS`).
///
/// # Errors
///
/// When SQLite refuses one.
pub(crate) fn register(connection: &mut SqliteConnection) -> QueryResult<()> {
    sum_decimal_utils::register_impl::<SumDecimal, _>(connection)?;
    conflicts_utils::register_impl(connection, |a: String, b: String| {
        tasks::Topic::parse(&a)
            .zip(tasks::Topic::parse(&b))
            .is_some_and(|(a, b)| a.conflicts(&b))
    })?;
    blocks_utils::register_impl(
        connection,
        |id: i64, status: i32, run_at: i64, attempts: i32, job: i64, now: i64| {
            let unsigned = |value: i64| u64::try_from(value).unwrap_or(0);
            tasks::blocks(
                tasks::Id(unsigned(id)),
                crate::models::task::status_of(status),
                unsigned(run_at),
                u32::try_from(attempts).unwrap_or(0),
                tasks::Id(unsigned(job)),
                unsigned(now),
            )
        },
    )
}

/// What a list query reads: at most `limit` rows after key `after` in the
/// list's order (from the first when `None`). Keyset, never `OFFSET`.
#[derive(Clone, Debug)]
pub(crate) struct Page<K> {
    pub(crate) after: Option<K>,
    pub(crate) limit: i64,
}

/// An item of a keyset-paged list: its key in the list's order, as the
/// cursor names it.
pub(crate) trait Keyed {
    fn key(&self) -> String;
}

/// A page of keyed items: the cursor of the next page, `None` unless full.
pub(crate) trait Paged {
    fn cursor(&self, limit: i64) -> Option<String>;
}

impl<T: Keyed> Paged for [T] {
    fn cursor(&self, limit: i64) -> Option<String> {
        (i64::try_from(self.len()) == Ok(limit))
            .then(|| self.last().map(Keyed::key))
            .flatten()
    }
}

/// A tag's key is itself.
impl Keyed for String {
    fn key(&self) -> String {
        self.clone()
    }
}

/// The one SQLite file's bounded pool. Clones share the pool.
#[derive(Clone)]
pub(crate) struct Db {
    pool: Pool<ConnectionManager<SqliteConnection>>,
}

impl Db {
    /// Opens the SQLite file at `path` (absolute). A new file (absent or
    /// empty) is created first: its folder, then the schema and its starting
    /// rows ([`Db::migrate`]), logged `app.db.created`. An existing file is
    /// opened as it is.
    ///
    /// # Errors
    ///
    /// When the path is relative, the file is not readable, its folder is
    /// not created, the pool does not open or the schema does not apply.
    pub(crate) async fn open_or_create(path: &Path) -> anyhow::Result<Self> {
        ensure!(path.is_absolute(), "sqlite path must be absolute");
        let new = match std::fs::metadata(path) {
            Ok(file) => file.len() == 0,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error).context("sqlite file is not readable"),
        };
        if !new {
            return Self::open(path);
        }
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder).context("sqlite folder was not created")?;
        }
        let db = Self::open(path)?;
        db.migrate().await?;
        tracing::info!(target: crate::events::APP_DB_CREATED, path = %path.display());
        Ok(db)
    }

    /// Opens or creates the SQLite file at `path`; applies no schema.
    ///
    /// # Errors
    ///
    /// When the pool does not open.
    fn open(path: &Path) -> anyhow::Result<Self> {
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

    /// Creates the schema (`apps/server/init.sql`) and its starting rows
    /// (`apps/server/seed.sql`) in one write on this (new) file.
    ///
    /// # Errors
    ///
    /// When the schema or the rows do not apply.
    async fn migrate(&self) -> anyhow::Result<()> {
        self.write(|connection| {
            connection.batch_execute(include_str!("../init.sql"))?;
            Ok(connection.batch_execute(include_str!("../seed.sql"))?)
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

#[cfg(test)]
mod tests {
    use diesel::{prelude::*, sql_types::Text};

    #[derive(QueryableByName)]
    struct Sum {
        #[diesel(sql_type = Text)]
        sum: String,
    }

    /// The sum of `values` (a JSON list of texts) by `SUM_DECIMAL`.
    fn sum(values: &str) -> QueryResult<String> {
        let mut connection = SqliteConnection::establish(":memory:").unwrap();
        super::register(&mut connection)?;
        diesel::sql_query(
            "
            SELECT SUM_DECIMAL(value) AS sum
            FROM json_each(?1)",
        )
        .bind::<Text, _>(values)
        .get_result::<Sum>(&mut connection)
        .map(|row| row.sum)
    }

    #[test]
    fn sum_decimal_is_exact_and_null_on_a_bad_value() {
        assert_eq!(
            sum(r#"["18446744073709551616", "18446744073709551617"]"#).unwrap(),
            "36893488147419103233"
        );
        assert_eq!(sum("[]").unwrap(), "0");
        assert!(sum(r#"["1", "x"]"#).is_err());
    }
}
