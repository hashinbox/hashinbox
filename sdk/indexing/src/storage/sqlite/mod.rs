//! SQLite repository over one file and a bounded connection pool.
//! One repository serves all networks; SQL predicates isolate each operation.
//! Each write reads its expected state and changes rows in one BEGIN IMMEDIATE
//! transaction. Whole operations run on blocking workers. Read snapshots use
//! separate pooled connections under WAL while one writer commits.
//! The same repository contracts run against SQLite and Memory.

mod activity;
mod balance;
mod blocks;
mod effects;
mod evidence;
mod history;
mod movements;
mod outputs;
mod progress;
mod schema;
mod steps;
mod tables;
mod watches;

use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

use crate::{IndexError, IndexErrorKind, Networked};

use tables::Tables;

/// An opened SQLite file and its bounded connection pool. Each operation borrows
/// one connection and runs entirely on a blocking worker. WAL permits readers
/// alongside the single writer. Clones share the pool.
pub struct Database {
    pool: Arc<Pool>,
}

/// The connections one open file lends, and the callers waiting for one.
///
/// A bounded pool rather than one connection per caller: every connection
/// carries its own page cache and its own file descriptor, and past the
/// machine's parallelism another reader buys no concurrency, only memory.
struct Pool {
    path: PathBuf,
    ceiling: usize,
    lending: Mutex<Lending>,
    returned: Condvar,
}

/// What a [`Pool`] is holding and what it has lent out.
struct Lending {
    /// Connections open, configured, and nobody's.
    idle: Vec<Connection>,
    /// Connections lent and not yet returned.
    outstanding: usize,
}

/// One connection borrowed from a [`Pool`] for the length of one operation,
/// returned to it when this is dropped.
struct Lease {
    pool: Arc<Pool>,
    /// `Some` for this lease's whole life; `None` only while [`Drop`] moves
    /// the connection back into the pool.
    connection: Option<Connection>,
}

/// SQLite storage for every network in one file.
///
/// Cheap to clone; every clone shares the file. Nothing here is bound to a
/// network: the operation says which one it is about.
#[derive(Clone)]
pub struct Repository {
    database: Database,
}

impl Clone for Database {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
        }
    }
}

impl Database {
    /// Opens or creates one SQLite database file and applies every schema step
    /// it is behind.
    ///
    /// # Errors
    ///
    /// `Store` when the path is not absolute, when the file belongs to another
    /// application or to a newer build's schema, or when SQLite cannot open it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IndexError> {
        Self::lending(path, Pool::ceiling())
    }

    /// Opens with an explicit ceiling on the connections one file lends.
    ///
    /// Production takes [`Pool::ceiling`]; a ceiling of one is the shared
    /// connection this pool replaced, kept so the measurement that justifies
    /// it can be run again.
    fn lending(path: impl AsRef<Path>, ceiling: usize) -> Result<Self, IndexError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(store(
                "sqlite path must be absolute; resolve application paths at composition",
            ));
        }
        let connection = Pool::connect(path)?;
        Self::migrate(&connection)?;
        Ok(Self {
            pool: Arc::new(Pool {
                path: path.to_path_buf(),
                ceiling: ceiling.max(1),
                lending: Mutex::new(Lending {
                    idle: vec![connection],
                    outstanding: 0,
                }),
                returned: Condvar::new(),
            }),
        })
    }

    /// Runs one whole operation off the async executor, on a connection of its
    /// own for as long as it runs.
    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, IndexError> + Send + 'static,
    ) -> Result<T, IndexError> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let mut lease = Pool::lease(&pool)?;
            operation(lease.connection())
        })
        .await
        .map_err(|error| {
            IndexError::new(
                IndexErrorKind::Store,
                format!("sqlite blocking operation did not finish: {error}"),
                true,
            )
        })?
    }

    /// Runs a conditional mutation in one BEGIN IMMEDIATE transaction.
    /// Reads and writes share the write lock; an error rolls back the whole operation.
    async fn transaction<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, IndexError> + Send + 'static,
    ) -> Result<T, IndexError> {
        self.blocking(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| failure(&error, "sqlite could not begin a write transaction"))?;
            let value = operation(&transaction)?;
            transaction
                .commit()
                .map_err(|error| failure(&error, "sqlite could not commit"))?;
            Ok(value)
        })
        .await
    }
}

impl Pool {
    /// How many connections one open file lends at once.
    ///
    /// The write-ahead log lets readers run alongside the one writer, so the
    /// useful ceiling is how many of them the machine can actually run at
    /// once. Never below two: with one connection the writer and a reader take
    /// turns on the same handle, which is the contention the pool exists to
    /// remove.
    fn ceiling() -> usize {
        std::thread::available_parallelism()
            .map_or(4, NonZeroUsize::get)
            .clamp(2, 16)
    }

    /// One connection of `pool`, waiting for a returned one when every
    /// connection the ceiling allows is already lent.
    fn lease(pool: &Arc<Self>) -> Result<Lease, IndexError> {
        let mut lending = pool.held()?;
        loop {
            if let Some(connection) = lending.idle.pop() {
                lending.outstanding += 1;
                return Ok(Lease::new(pool, connection));
            }
            if lending.outstanding < pool.ceiling {
                lending.outstanding += 1;
                drop(lending);
                // Opening a file is slow enough to be worth doing outside the
                // lock, so the slot is claimed first and given back if the
                // open fails.
                return Self::connect(&pool.path)
                    .map(|connection| Lease::new(pool, connection))
                    .inspect_err(|_| pool.give_back(None));
            }
            lending = pool
                .returned
                .wait(lending)
                .map_err(|_| store("sqlite connection pool is poisoned"))?;
        }
    }

    /// One new connection to `path`, with this adapter's pragmas on it.
    ///
    /// Every pragma [`Database::configure`] sets is per-connection, so a
    /// connection the pool opens later is configured exactly as the first one
    /// was; only the schema steps belong to the file and run once.
    fn connect(path: &Path) -> Result<Connection, IndexError> {
        let connection =
            Connection::open(path).map_err(|error| failure(&error, "sqlite open failed"))?;
        Database::configure(&connection)?;
        Ok(connection)
    }

    /// Takes a lent connection back, or releases the slot of one that was
    /// never opened, and wakes one caller waiting for it.
    fn give_back(&self, connection: Option<Connection>) {
        if let Ok(mut lending) = self.lending.lock() {
            lending.outstanding = lending.outstanding.saturating_sub(1);
            lending.idle.extend(connection);
        }
        self.returned.notify_one();
    }

    fn held(&self) -> Result<std::sync::MutexGuard<'_, Lending>, IndexError> {
        self.lending
            .lock()
            .map_err(|_| store("sqlite connection pool is poisoned"))
    }
}

impl Lease {
    fn new(pool: &Arc<Pool>, connection: Connection) -> Self {
        Self {
            pool: Arc::clone(pool),
            connection: Some(connection),
        }
    }

    /// The borrowed connection.
    fn connection(&mut self) -> &mut Connection {
        self.connection
            .as_mut()
            .unwrap_or_else(|| unreachable!("a lease holds its connection until it is dropped"))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.pool.give_back(self.connection.take());
    }
}

impl Repository {
    /// Speaks the domain over an open SQLite file. No I/O.
    #[must_use]
    pub const fn new(storage: Database) -> Self {
        Self { database: storage }
    }

    /// The statements of one network, ready to cross into the blocking hop.
    ///
    /// A [`Repository`] must never be moved into one: it holds the pool, and
    /// the hop it would be moved into is the one already holding a connection
    /// of that pool. What crosses is the network and nothing else.
    fn tables(&self, network: &impl Networked) -> Result<Tables, IndexError> {
        let network = network.canonical();
        if network.chain.as_str().trim().is_empty() || network.name.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "an indexed network must contain a chain and name",
                false,
            ));
        }
        Ok(Tables::new(network))
    }
}

/// A SQLite failure, classified the way callers already retry.
///
/// A lock another connection holds past the busy timeout is the `Conflict` a
/// caller re-reads and retries; an I/O fault is a retryable store failure;
/// everything else is a store failure that another attempt cannot fix.
fn failure(error: &rusqlite::Error, context: &str) -> IndexError {
    let (kind, retryable) = match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            (IndexErrorKind::Conflict, true)
        }
        Some(ErrorCode::SystemIoFailure | ErrorCode::CannotOpen) => (IndexErrorKind::Store, true),
        _ => (IndexErrorKind::Store, false),
    };
    IndexError::new(kind, format!("{context}: {error}"), retryable)
}

/// A failure of this adapter rather than of SQLite; retrying cannot fix it.
fn store(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

/// A condition the caller stated that the file no longer satisfies; nothing
/// was written, so the caller re-reads and tries again.
fn conflict(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Conflict, message, true)
}

#[cfg(test)]
mod database_test {
    use tempfile::TempDir;

    use super::*;
    use crate::{ChainName, Network};

    /// The network every scenario in this module's siblings is bound to.
    pub(super) fn network() -> Network {
        Network::new(ChainName::new("chain"), "testnet")
    }

    /// SQLite work runs on `tokio::task::spawn_blocking`, so a scenario needs a
    /// Tokio runtime rather than a bare executor.
    pub(super) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .build()
                    .expect("test runtime")
            })
            .block_on(future)
    }

    /// One repository over a fresh file named `name`.
    ///
    /// It serves every network in that file: [`network`] is what the scenarios
    /// pass to the operations, not what the repository is bound to.
    pub(super) fn open(directory: &TempDir, name: &str) -> Repository {
        let database = Database::open(directory.path().join(name)).expect("sqlite database");
        Repository::new(database)
    }

    /// One repository over a fresh file that lends exactly `ceiling`
    /// connections: a ceiling of one is the single shared connection the pool
    /// replaced.
    pub(super) fn narrow(directory: &TempDir, name: &str, ceiling: usize) -> Repository {
        let database =
            Database::lending(directory.path().join(name), ceiling).expect("sqlite database");
        Repository::new(database)
    }

    /// A relative path would resolve against whatever directory the process
    /// happens to be in, so it is refused at the boundary rather than opened.
    #[test]
    fn a_relative_path_is_refused() {
        let error = Database::open("index.db").map(drop).expect_err("relative");
        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(error.message.contains("must be absolute"), "{error}");
    }

    /// Two operations in flight hold two connections, and both come back.
    #[test]
    fn concurrent_operations_hold_a_connection_each() {
        let directory = TempDir::new().expect("temporary directory");
        let database = Database::open(directory.path().join("pool.db")).expect("sqlite database");
        let first = Pool::lease(&database.pool).expect("the first connection");
        let second = Pool::lease(&database.pool).expect("a second connection");
        assert_eq!(database.pool.held().expect("pool").outstanding, 2);
        drop((first, second));
        let lending = database.pool.held().expect("pool");
        assert_eq!((lending.idle.len(), lending.outstanding), (2, 0));
    }

    /// The pool never opens more than its ceiling allows: a third caller of a
    /// two-connection file waits for one of the two to come back.
    #[test]
    fn a_caller_waits_rather_than_opening_past_the_ceiling() {
        let directory = TempDir::new().expect("temporary directory");
        let database =
            Database::lending(directory.path().join("ceiling.db"), 2).expect("sqlite database");
        let held = (
            Pool::lease(&database.pool).expect("first"),
            Pool::lease(&database.pool).expect("second"),
        );
        let pool = Arc::clone(&database.pool);
        let waiting = std::thread::spawn(move || Pool::lease(&pool).map(drop));
        // The waiter cannot proceed until one is returned, so returning both
        // is what lets it finish at all.
        drop(held);
        waiting.join().expect("the waiting thread").expect("third");
        assert_eq!(database.pool.held().expect("pool").idle.len(), 2);
    }

    /// The measurement the pool exists for, as a test rather than a benchmark:
    /// a read runs to completion while a write transaction is open on the same
    /// file. One shared connection cannot do this -- the reader would wait for
    /// the writer's connection -- so the writer here waits for the *reader*,
    /// and reports whether it was ever released.
    #[test]
    fn a_read_runs_while_a_write_transaction_is_open() {
        use std::sync::mpsc;

        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "alongside.db");
        let (opened, waiting) = mpsc::channel();
        let (finished, reading) = mpsc::channel();
        let (outcome, reported) = mpsc::channel();
        let database = repository.database.clone();
        let writing = std::thread::spawn(move || {
            block_on(database.transaction(move |_| {
                opened.send(()).expect("the reader is there");
                let waited = reading.recv_timeout(std::time::Duration::from_secs(10));
                outcome.send(waited.is_ok()).expect("the reader is there");
                Ok(())
            }))
        });
        waiting.recv().expect("the write transaction is open");
        let tables = repository.tables(&network()).expect("this network");
        block_on(
            repository
                .database
                .blocking(move |connection| tables.checkpoint(connection)),
        )
        .expect("a read while the write transaction is open");
        finished.send(()).expect("the writer is there");
        assert!(
            reported.recv().expect("the writer reports"),
            "the writer was released by its own timeout rather than by the \
             reader: the read did not finish alongside it",
        );
        writing.join().expect("the writing thread").expect("commit");
    }
}
