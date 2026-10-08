//! SQLite repository over one file and its `sqlx` pool.
//! One repository serves all networks; SQL predicates isolate each operation.
//! Each write reads its expected state and changes rows in one BEGIN IMMEDIATE
//! transaction. Read snapshots use separate pooled connections under WAL while
//! one writer commits; every connection does its SQLite work on a worker thread
//! of its own, never on the async executor.
//! The same repository contracts run against SQLite and Memory.
//!
//! Every statement is its table's model's (`models/`). A port's file only
//! orchestrates: it opens the transaction or snapshot, calls the models, and
//! commits or releases.

use crate::storage::Probe;
use crate::storage::watch::redefinition;
use crate::{
    BlockRef, BoxFuture, IndexError, IndexErrorKind, IndexedOutput, Inspection, Location, Network,
    Networked, OutputCursor, OutputPage, OutputRequest, OutputStore, Registration, TransactionRef,
    TransactionStore, WalletRegistry, WatchProgress,
};
use models::{Output, Wallet};
use sqlx::Connection as _;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions as _, Sqlite, SqliteConnection, SqlitePool, Transaction};
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

mod commit;
mod models;
mod watches;

/// How long a statement waits for another connection's lock before it fails
/// as a retryable `Conflict`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an operation waits for a pooled connection before it fails as a
/// retryable `Store` error.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Prepared statements each connection keeps: every statement this store runs,
/// with room to spare. A commit's many-row statements are one text per chunk
/// width (`models::Chunks`) per statement, about eighty of them.
const STATEMENT_CACHE: usize = 256;

/// An opened SQLite file and its bounded connection pool. WAL permits readers
/// alongside the single writer. Clones share the pool.
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

/// SQLite storage for every network in one file.
///
/// Cheap to clone; every clone shares the file. Nothing here is bound to a
/// network: the operation says which one it is about.
#[derive(Clone)]
pub struct Repository {
    database: Database,
}

impl Database {
    /// Opens or creates the SQLite file at `path` holding the indexing schema
    /// alone. Requires a Tokio runtime.
    ///
    /// An empty file is given the schema and stamped with this store's
    /// identity; a file stamped by this very schema is opened as it is. There
    /// are no migrations: any other file is refused, and a schema change needs
    /// a fresh file.
    ///
    /// # Errors
    ///
    /// A non-retryable `Store` error when the path is not absolute, when the
    /// file refuses write-ahead logging, or when it belongs to another
    /// application or schema version (the message says to delete the database
    /// file); otherwise the SQLite failure, classified.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, IndexError> {
        Self::connect(path.as_ref(), None, Self::ceiling()).await
    }

    /// [`Database::open`] for an application that keeps tables of its own in
    /// the same file: `schema` is applied beside the indexing schema when the
    /// file is created, and the file's identity covers both, so a file created
    /// with another `schema` (or with none) is refused. Requires a Tokio
    /// runtime.
    ///
    /// # Errors
    ///
    /// As [`Database::open`]; a `schema` that does not apply fails the open
    /// and leaves the file empty.
    pub async fn open_with(path: impl AsRef<Path>, schema: &str) -> Result<Self, IndexError> {
        Self::connect(path.as_ref(), Some(schema), Self::ceiling()).await
    }

    /// The file's pool, for an application's statements against its own
    /// tables. Indexing's tables are the repository's; write them through it.
    #[must_use]
    pub const fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The machine's parallelism, clamped to 2..=16.
    ///
    /// Never below two connections: with one, the writer and a reader take
    /// turns on the same handle, which is the contention a pool removes. Never
    /// far above the cores: every connection carries its own page cache and
    /// worker thread, and past the machine's parallelism another reader buys
    /// memory, not concurrency.
    fn ceiling() -> u32 {
        let parallelism = std::thread::available_parallelism().map_or(4, NonZeroUsize::get);
        u32::try_from(parallelism.clamp(2, 16)).unwrap_or(2)
    }

    /// Opens `path` as a pool of at most `ceiling` connections and checks the
    /// file's identity.
    ///
    /// Every connection gets the busy timeout, `synchronous = FULL`,
    /// `fullfsync` (what makes `FULL` reach the disk on macOS), foreign keys,
    /// and write-ahead logging, which is what lets readers run beside the one
    /// writer. Temporary storage is memory: a commit writes many rows to a
    /// statement, each such statement keeps a statement journal in case it must
    /// be undone alone, and a journal past 64 KiB would otherwise spill to a
    /// temporary file created and deleted inside the commit. No connection is
    /// closed for idleness or age: an embedded file connection does not go
    /// stale.
    async fn connect(path: &Path, owner: Option<&str>, ceiling: u32) -> Result<Self, IndexError> {
        if !path.is_absolute() {
            return Err(store(
                "sqlite path must be absolute; resolve application paths at composition",
            ));
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .pragma("fullfsync", "ON")
            .pragma("temp_store", "MEMORY")
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT)
            .statement_cache_capacity(STATEMENT_CACHE)
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(ceiling)
            .min_connections(0)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .test_before_acquire(false)
            .connect_with(options)
            .await
            .map_err(|error| failure(&error, "sqlite open failed"))?;
        Identity::new(owner).establish(&pool).await?;
        Ok(Self { pool })
    }

    /// One connection for one read that needs no snapshot.
    async fn connection(&self) -> Result<PoolConnection<Sqlite>, IndexError> {
        self.pool
            .acquire()
            .await
            .map_err(|error| failure(&error, "sqlite could not lend a connection"))
    }

    /// One `BEGIN IMMEDIATE` transaction: it holds the file's write lock from
    /// its first statement, so what it reads is still true when it writes.
    /// [`commit`] ends it; dropping it rolls the whole of it back.
    async fn write(&self) -> Result<Transaction<'static, Sqlite>, IndexError> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| failure(&error, "sqlite could not begin a write transaction"))
    }

    /// One read transaction: every statement sees the snapshot its first one
    /// fixed, while the writer goes on committing. [`release`] ends it.
    async fn read(&self) -> Result<Transaction<'static, Sqlite>, IndexError> {
        self.pool
            .begin()
            .await
            .map_err(|error| failure(&error, "sqlite could not begin a read"))
    }

    /// One read snapshot of `network`, and the checkpoint it is consistent
    /// with.
    ///
    /// One value, because the checkpoint read is the transaction's *first*
    /// statement and that is what fixes the snapshot every later page is read
    /// from: handing back the pair is what stops a read being written that pairs
    /// a page with a checkpoint no single snapshot held.
    async fn snapshot(
        &self,
        network: &Network,
    ) -> Result<(Transaction<'static, Sqlite>, Option<BlockRef>), IndexError> {
        let mut read = self.read().await?;
        let checkpoint = checkpoint(&mut read, network).await?;
        Ok((read, checkpoint))
    }
}

/// `network`'s checkpoint, or `None` before its first commit.
async fn checkpoint(
    connection: &mut sqlx::SqliteConnection,
    network: &Network,
) -> Result<Option<BlockRef>, IndexError> {
    found(
        models::NetworkCheckpoint::get(connection, network).await,
        "failed to read the checkpoint",
    )?
    .map(BlockRef::try_from)
    .transpose()
}

/// The one row a model's `get_*` answered, or `None` when there was none:
/// what a port asks where absence is a legitimate answer. Any other failure is
/// classified as [`failure`] classifies it.
fn found<T>(fetched: Result<T, sqlx::Error>, context: &str) -> Result<Option<T>, IndexError> {
    match fetched {
        Ok(row) => Ok(Some(row)),
        Err(sqlx::Error::RowNotFound) => Ok(None),
        Err(error) => Err(failure(&error, context)),
    }
}

/// Commits a [`Database::write`].
///
/// An error here is not proof that nothing was written: a commit whose answer
/// was lost may still have landed, which is why every write this store makes
/// is harmless to repeat.
async fn commit(transaction: Transaction<'_, Sqlite>) -> Result<(), IndexError> {
    transaction
        .commit()
        .await
        .map_err(|error| failure(&error, "sqlite could not commit"))
}

/// Ends a [`Database::read`] explicitly, so the connection goes back to the
/// pool with no snapshot held open.
async fn release(read: Transaction<'static, Sqlite>) -> Result<(), IndexError> {
    read.rollback()
        .await
        .map_err(|error| failure(&error, "sqlite could not end a read"))
}

impl Repository {
    /// Speaks the domain over an open SQLite file. No I/O.
    #[must_use]
    pub fn new(storage: Database) -> Self {
        Self { database: storage }
    }

    /// The network an operation is about, refused when it names no chain or
    /// no network: every statement is framed by both.
    fn network(network: &impl Networked) -> Result<Network, IndexError> {
        let network = network.canonical();
        if network.chain.as_str().trim().is_empty() || network.name.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "an indexed network must contain a chain and name",
                false,
            ));
        }
        Ok(network)
    }
}

/// A SQLite or pool failure, with what was being attempted, classified the way
/// callers already retry.
///
/// A lock held past the busy timeout is the `Conflict` a caller re-reads and
/// retries. A file that cannot be reached, a pool with no connection to lend
/// and a connection worker that stopped are retryable `Store` failures. A
/// model's own refusal of a row it read keeps its kind and message. Every other
/// conversion failure is reduced to what failed, so no message carries a bound
/// parameter or a stored value. A row that was expected and is missing — a
/// `get_*` whose absence the port did not turn into its own answer — and
/// anything else is a non-retryable `Store` failure.
fn failure(error: &sqlx::Error, context: &str) -> IndexError {
    match error {
        sqlx::Error::Database(database) => {
            let code = database
                .code()
                .and_then(|code| code.parse::<i32>().ok())
                .unwrap_or_default();
            let message = format!("{context}: {}", database.message());
            // The primary result code is the low byte of the extended one.
            match code & 0xff {
                // SQLITE_BUSY, SQLITE_LOCKED
                5 | 6 => IndexError::new(IndexErrorKind::Conflict, message, true),
                // SQLITE_IOERR, SQLITE_CANTOPEN
                10 | 14 => IndexError::new(IndexErrorKind::Store, message, true),
                _ => IndexError::new(IndexErrorKind::Store, message, false),
            }
        }
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::WorkerCrashed => {
            IndexError::new(IndexErrorKind::Store, format!("{context}: {error}"), true)
        }
        sqlx::Error::Decode(source) | sqlx::Error::ColumnDecode { source, .. }
            if source.is::<IndexError>() =>
        {
            source
                .downcast_ref::<IndexError>()
                .cloned()
                .unwrap_or_else(|| store(context))
        }
        sqlx::Error::Decode(_)
        | sqlx::Error::ColumnDecode { .. }
        | sqlx::Error::Encode(_)
        | sqlx::Error::TypeNotFound { .. }
        | sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnIndexOutOfBounds { .. } => store(format!(
            "{context}: a value did not convert to or from its column type"
        )),
        sqlx::Error::RowNotFound => store(format!("{context}: an expected row is missing")),
        other => store(format!("{context}: {other}")),
    }
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

    /// One repository over a fresh file named `name`.
    ///
    /// It serves every network in that file: [`network`] is what the scenarios
    /// pass to the operations, not what the repository is bound to.
    pub(super) async fn open(directory: &TempDir, name: &str) -> Repository {
        let database = Database::open(directory.path().join(name))
            .await
            .expect("sqlite database");
        Repository::new(database)
    }

    /// One repository over a fresh file that lends exactly `ceiling`
    /// connections: a ceiling of one is a single shared connection.
    pub(super) async fn narrow(directory: &TempDir, name: &str, ceiling: u32) -> Repository {
        let database = Database::connect(&directory.path().join(name), None, ceiling)
            .await
            .expect("sqlite database");
        Repository::new(database)
    }

    /// A relative path would resolve against whatever directory the process
    /// happens to be in, so it is refused at the boundary rather than opened.
    #[tokio::test]
    async fn a_relative_path_is_refused() {
        let error = Database::open("index.db")
            .await
            .map(drop)
            .expect_err("relative");
        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(error.message.contains("must be absolute"), "{error}");
    }

    /// The measurement the pool exists for, as a test rather than a benchmark:
    /// a read runs to completion while a write transaction is open on the same
    /// file. One shared connection cannot do this -- the reader would wait for
    /// the writer's connection.
    #[tokio::test]
    async fn a_read_runs_while_a_write_transaction_is_open() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "alongside.db").await;
        let write = repository.database.write().await.expect("the write lock");
        let read = async {
            let mut connection = repository.database.connection().await?;
            checkpoint(&mut connection, &network()).await
        };
        let read = tokio::time::timeout(Duration::from_secs(10), read)
            .await
            .expect("the read finished while the write transaction was open");
        assert_eq!(read.expect("a read beside the writer"), None);
        commit(write).await.expect("commit");
    }

    /// Every failure is classified by whether a retry can help, and none
    /// quotes a value: a held lock is a retryable conflict, a conversion
    /// failure says only what failed, and a model's own refusal arrives as it
    /// was raised.
    #[test]
    fn failures_are_classified_and_redacted() {
        let decoded = failure(
            &sqlx::Error::Decode("s3cret is not a number".into()),
            "failed to read a row",
        );
        assert_eq!(decoded.kind, IndexErrorKind::Store);
        assert!(!decoded.retryable);
        assert!(!decoded.message.contains("s3cret"), "{decoded}");

        let refused = store("a stored movement has an unknown kind");
        let raised = failure(
            &sqlx::Error::Decode(Box::new(refused.clone())),
            "failed to read a row",
        );
        assert_eq!(raised, refused);

        let timed_out = failure(&sqlx::Error::PoolTimedOut, "lend");
        assert_eq!(timed_out.kind, IndexErrorKind::Store);
        assert!(timed_out.retryable);
    }

    /// A second writer meets the first one's lock: past the busy timeout it is
    /// the retryable `Conflict` a caller re-reads and retries.
    #[tokio::test]
    async fn a_held_write_lock_is_a_retryable_conflict() {
        use sqlx::Connection as _;

        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("busy.db");
        let first = Database::open(&path).await.expect("first");
        let held = first.write().await.expect("the write lock");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .busy_timeout(Duration::from_millis(50));
        let mut second = sqlx::SqliteConnection::connect_with(&options)
            .await
            .expect("second connection");
        let error = sqlx::raw_sql("BEGIN IMMEDIATE")
            .execute(&mut second)
            .await
            .expect_err("the lock is held");
        let classified = failure(&error, "begin");
        assert_eq!(classified.kind, IndexErrorKind::Conflict);
        assert!(classified.retryable);
        drop(held);
    }
}

// The file's one schema (`schema.sql`) and the identity that says it holds
// it.
//
// There are no migrations. A file is this store's when its
// `PRAGMA application_id` is [`APPLICATION_ID`] and its `PRAGMA user_version`
// is the fingerprint of the exact schema text it was created with — the
// indexing schema, and the owner schema of an application that keeps its own
// tables beside it. An empty file is given the schema and stamped; any other
// file is refused, and a schema change, which changes the fingerprint, needs a
// fresh file.

/// The indexing tables and views, applied whole to an empty file.
const SCHEMA: &str = include_str!("schema.sql");

/// `PRAGMA application_id` of every file this store creates. A file carrying
/// any other value is some other application's and is refused.
const APPLICATION_ID: i32 = 0x7733_5142;

/// FNV-1a: the offset basis and prime of its 32-bit form.
const OFFSET: u32 = 0x811c_9dc5;
const PRIME: u32 = 0x0100_0193;

/// The fingerprint of the indexing schema alone.
const INDEXING: u32 = fingerprint(OFFSET, SCHEMA);

/// What a refused file is told to do: the store never rewrites a file it did
/// not create.
const REMEDY: &str = "delete the database file";

/// `hash` continued over `text`, FNV-1a.
const fn fingerprint(mut hash: u32, text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u32;
        hash = hash.wrapping_mul(PRIME);
        index += 1;
    }
    hash
}

/// The identity a file opened with one set of schema texts must carry.
pub(super) struct Identity<'a> {
    owner: Option<&'a str>,
}

impl<'a> Identity<'a> {
    /// The indexing schema, and `owner`'s beside it when there is one.
    pub(super) const fn new(owner: Option<&'a str>) -> Self {
        Self { owner }
    }

    /// The `PRAGMA user_version` this identity's file carries: the fingerprint
    /// of every schema text, never zero, which marks an unstamped file.
    fn version(&self) -> i32 {
        let hash = match self.owner {
            Some(owner) => fingerprint(INDEXING, owner),
            None => INDEXING,
        };
        match i32::from_ne_bytes(hash.to_ne_bytes()) {
            0 => 1,
            version => version,
        }
    }

    /// Accepts a file carrying this identity, gives an empty unmarked one the
    /// schema and the identity, and refuses everything else — in one
    /// `BEGIN IMMEDIATE` transaction, so two processes opening one empty file
    /// never both create it.
    ///
    /// Both pragmas are zero in *any* database nobody stamped, an unrelated
    /// application's included, so an unmarked file is adopted only when it is
    /// empty. Otherwise somebody else's data would be stamped as this store's.
    pub(super) async fn establish(&self, pool: &SqlitePool) -> Result<(), IndexError> {
        let mut connection = pool
            .acquire()
            .await
            .map_err(|error| failure(&error, "sqlite could not lend a connection"))?;
        let mode: String = pragma(&mut connection, "journal_mode").await?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(store(format!(
                "sqlite refused write-ahead logging and stayed in {mode} mode"
            )));
        }
        let mut write = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| failure(&error, "sqlite could not begin a write transaction"))?;
        let found: i32 = pragma(&mut write, "application_id").await?;
        let version: i32 = pragma(&mut write, "user_version").await?;
        if found == APPLICATION_ID && version == self.version() {
            return commit(write).await;
        }
        if found == APPLICATION_ID {
            return Err(store(format!(
                "sqlite file carries another schema version; {REMEDY}"
            )));
        }
        if found != 0 || version != 0 {
            return Err(store(format!(
                "sqlite file belongs to another application; use another path or {REMEDY}"
            )));
        }
        let objects: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master")
            .fetch_one(&mut *write)
            .await
            .map_err(|error| failure(&error, "failed to read the sqlite object list"))?;
        if objects > 0 {
            return Err(store(format!(
                "sqlite file carries tables this build did not create; use another path or \
                 {REMEDY}"
            )));
        }
        self.create(&mut write).await?;
        commit(write).await
    }

    /// Applies every schema text to the empty file and stamps it.
    async fn create(&self, write: &mut SqliteConnection) -> Result<(), IndexError> {
        for schema in [Some(SCHEMA), self.owner].into_iter().flatten() {
            sqlx::raw_sql(schema)
                .execute(&mut *write)
                .await
                .map_err(|error| failure(&error, "failed to apply the sqlite schema"))?;
        }
        // Both values are this module's own integers; nothing a caller
        // supplies reaches the statement.
        let stamp = format!(
            "PRAGMA application_id = {APPLICATION_ID}; PRAGMA user_version = {};",
            self.version()
        );
        sqlx::raw_sql(&stamp)
            .execute(&mut *write)
            .await
            .map(drop)
            .map_err(|error| failure(&error, "failed to stamp the sqlite file"))
    }
}

/// One `PRAGMA` read as a value. `name` is this module's own literal.
async fn pragma<T>(connection: &mut SqliteConnection, name: &str) -> Result<T, IndexError>
where
    T: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    sqlx::query_scalar(&format!("PRAGMA {name}"))
        .fetch_one(connection)
        .await
        .map_err(|error| failure(&error, "failed to read a sqlite pragma"))
}

#[cfg(test)]
mod schema_test {
    use sqlx::Executor as _;
    use tempfile::TempDir;

    use super::*;
    use super::{
        Database,
        models::{Output, Wallet},
    };
    use crate::IndexErrorKind;

    /// A raw connection to `path`, outside any pool and any identity check.
    async fn raw(path: &std::path::Path) -> SqliteConnection {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        SqliteConnection::connect_with(&options)
            .await
            .expect("a raw connection")
    }

    /// `Database::open`'s refusal of `path`, which must tell the operator what
    /// to do about it.
    async fn refused(path: &std::path::Path) -> IndexError {
        let error = Database::open(path)
            .await
            .map(drop)
            .expect_err("a refused file");
        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "reopening cannot change the file");
        assert!(error.message.contains(REMEDY), "{error}");
        error
    }

    /// A file that is another application's, an unmarked file somebody else
    /// populated, and a file of another schema version are all refused rather
    /// than interpreted, each with the same remedy.
    #[tokio::test]
    async fn foreign_unmarked_and_other_version_files_are_refused() {
        let directory = TempDir::new().expect("temporary directory");
        let foreign = directory.path().join("foreign.db");
        sqlx::raw_sql("PRAGMA application_id = 12345; CREATE TABLE other (value TEXT);")
            .execute(&mut raw(&foreign).await)
            .await
            .expect("foreign schema");
        let error = refused(&foreign).await;
        assert!(
            error
                .message
                .starts_with("sqlite file belongs to another application")
        );

        // An unmarked stranger: both pragmas are zero, as they are in every
        // database nobody stamped, so emptiness is the only thing that tells it
        // apart from a file this build is about to create.
        let unmarked = directory.path().join("unmarked.db");
        sqlx::raw_sql("CREATE TABLE theirs (value TEXT);")
            .execute(&mut raw(&unmarked).await)
            .await
            .expect("somebody else's table");
        let error = refused(&unmarked).await;
        assert!(error.message.starts_with("sqlite file carries tables"));

        for version in [5, 99] {
            let other = directory.path().join(format!("version-{version}.db"));
            Database::open(&other).await.expect("a fresh file");
            sqlx::raw_sql(&format!("PRAGMA user_version = {version};"))
                .execute(&mut raw(&other).await)
                .await
                .expect("another version");
            let error = refused(&other).await;
            assert!(
                error
                    .message
                    .starts_with("sqlite file carries another schema version")
            );
        }

        // The application's own schema is part of the identity: a file
        // created with indexing's alone is not the one an owner opens.
        let owned = directory.path().join("owned.db");
        Database::open(&owned).await.expect("indexing alone");
        let error = Database::open_with(&owned, "CREATE TABLE mine (value TEXT);")
            .await
            .map(drop)
            .expect_err("another schema");
        assert!(error.message.contains(REMEDY), "{error}");
    }

    /// SQLite must accept the requested durability, concurrency and
    /// temporary-storage settings, on every pooled connection. In particular
    /// macOS needs fullfsync rather than an assumed drive-cache flush.
    #[tokio::test]
    async fn durability_and_concurrency_settings_are_the_ones_asked_for() {
        let directory = TempDir::new().expect("temporary directory");
        let database = Database::open(directory.path().join("pragmas.db"))
            .await
            .expect("open");
        let mut first = database.pool().acquire().await.expect("a connection");
        let mut second = database.pool().acquire().await.expect("another");
        for connection in [&mut first, &mut second] {
            let expected = [
                ("synchronous", 2_i64),
                ("fullfsync", 1),
                ("foreign_keys", 1),
                ("temp_store", 2),
            ];
            for (name, expected) in expected {
                let value: i64 = pragma(connection, name).await.expect("pragma");
                assert_eq!(value, expected, "{name}");
            }
            let mode: String = pragma(connection, "journal_mode").await.expect("pragma");
            assert_eq!(mode, "wal");
        }
    }

    /// Opening the same file twice creates it once, and a file this build
    /// created is stamped with both its identity and its schema fingerprint;
    /// an owner's file is stamped with the fingerprint of both texts.
    #[tokio::test]
    async fn a_reopened_file_is_stamped_once() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("index.db");
        for _ in 0..2 {
            Database::open(&path).await.expect("open");
        }
        let mut connection = raw(&path).await;
        let found: i32 = pragma(&mut connection, "application_id").await.expect("id");
        let version: i32 = pragma(&mut connection, "user_version")
            .await
            .expect("version");
        assert_eq!(found, APPLICATION_ID);
        assert_eq!(version, Identity::new(None).version());

        let owner = "CREATE TABLE mine (value TEXT);";
        let owned = directory.path().join("owned.db");
        for _ in 0..2 {
            Database::open_with(&owned, owner).await.expect("open");
        }
        let mut connection = raw(&owned).await;
        let version: i32 = pragma(&mut connection, "user_version")
            .await
            .expect("version");
        assert_eq!(version, Identity::new(Some(owner)).version());
        assert_ne!(version, Identity::new(None).version());
    }

    /// The `EXPLAIN QUERY PLAN` of `statement`, one line per step, with every
    /// parameter bound to null.
    async fn plan(connection: &mut SqliteConnection, statement: &str) -> String {
        let explained = format!("EXPLAIN QUERY PLAN {statement}");
        let prepared = connection
            .prepare(explained.as_str())
            .await
            .expect("a plannable statement");
        let parameters = match sqlx::Statement::parameters(&prepared) {
            Some(sqlx::Either::Right(count)) => count,
            _ => 0,
        };
        let mut query = sqlx::query(&explained);
        for _ in 0..parameters {
            query = query.bind(None::<i64>);
        }
        let rows = query.fetch_all(connection).await.expect("a plan");
        rows.iter()
            .map(|row| sqlx::Row::get::<String, _>(row, 3))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every read a page, the scan list or a commit makes is a keyed search:
    /// no scan, no materialized view and no sort the key does not give.
    ///
    /// A live-output page names `spent = 0` itself, which is what keeps the
    /// partial index `output_unspent` usable; without statistics SQLite still
    /// prefers the primary key for the page, whose columns are the index's own
    /// and need no second lookup per row (forcing the index measured no
    /// faster). The `history_entry` view is flattened into the join it is, and
    /// a history page's movements are one asset's key range, sorted within it.
    /// The address read takes its keys from `wallet_address` and only then each
    /// row, rather than walking the network's whole `wallet` key, and
    /// `archived_at IS NULL` filters the rows a search reaches without
    /// changing it.
    #[tokio::test]
    async fn reads_are_keyed_searches() {
        let directory = TempDir::new().expect("temporary directory");
        let database = Database::open(directory.path().join("plans.db"))
            .await
            .expect("open");
        let mut connection = database.pool().acquire().await.expect("a connection");
        let expected = [
            (
                Output::LIVE,
                "SEARCH output USING PRIMARY KEY (chain=? AND network=? AND address=? AND \
                 (transaction_id,output_index)>(?,?))",
            ),
            (
                "SELECT count(*) FROM output
                 WHERE chain = ?1 AND network = ?2 AND address = ?3 AND spent = 0",
                "SEARCH output USING COVERING INDEX output_unspent \
                 (chain=? AND network=? AND address=?)",
            ),
            (
                Wallet::OBSERVED,
                "SEARCH wallet USING PRIMARY KEY (chain=? AND network=?)",
            ),
            (
                Wallet::OBSERVED_BY_ADDRESS,
                "SEARCH wallet USING INDEX wallet_address (chain=? AND network=? AND address=?)",
            ),
            (
                Wallet::OBSERVED_BY_ID,
                "SEARCH wallet USING INDEX wallet_id (id=?)",
            ),
        ];
        for (statement, steps) in expected {
            assert_eq!(plan(&mut connection, statement).await, steps, "{statement}");
        }
    }

    /// The schema refuses the rows no model can write: an unknown status,
    /// direction or kind, and a fee, parent or creation written by halves.
    #[tokio::test]
    async fn the_schema_refuses_half_written_and_unknown_rows() {
        let directory = TempDir::new().expect("temporary directory");
        let database = Database::open(directory.path().join("checks.db"))
            .await
            .expect("open");
        let transaction = |key: i64, status: i64, fee: &str, parent: &str| {
            format!(
                "INSERT INTO \"transaction\" (chain, network, transaction_id, block_position,
                     block_height, block_hash, block_parent_position, block_parent_hash,
                     transaction_order, execution_status, fee_asset, fee_amount)
                 VALUES ('c', 'n', 't{key}', 1, 1, x'01', {parent}, 0, {status},
                     {fee})"
            )
        };
        let ledger = |key: i64, direction: i64, kind: i64| {
            format!(
                "INSERT INTO ledger (chain, network, address, asset, block_height,
                     transaction_id, movement_id, direction, block_position, movement_order,
                     kind, amount)
                 VALUES ('c', 'n', 'a', 'x', 1, 't', 'm{key}', {direction}, 1, 0,
                     {kind}, '1')"
            )
        };
        let output = |key: i64, creation: &str, spent: i64| {
            format!(
                "INSERT INTO output (chain, network, address, transaction_id, output_index,
                     asset, amount, evidence, created_height, coinbase, spent)
                 VALUES ('c', 'n', 'a', 't', {key}, {creation}, {spent})"
            )
        };
        let whole = "'x', '1', x'', 1, 0";
        for (accepted, statement) in [
            (true, transaction(0, 0, "'x', '1'", "1, x'00'")),
            (true, transaction(1, 1, "NULL, NULL", "NULL, NULL")),
            (false, transaction(2, 2, "NULL, NULL", "NULL, NULL")),
            (false, transaction(3, 0, "'x', NULL", "NULL, NULL")),
            (false, transaction(4, 0, "NULL, NULL", "1, NULL")),
            (true, ledger(0, 0, 6)),
            (false, ledger(1, 2, 0)),
            (false, ledger(2, 1, 7)),
            (true, output(0, whole, 0)),
            (true, output(1, "NULL, NULL, NULL, NULL, NULL", 1)),
            (false, output(2, "NULL, NULL, NULL, NULL, NULL", 0)),
            (false, output(3, whole, 2)),
            (false, output(4, "'x', '1', NULL, 1, 0", 1)),
            (false, output(5, "'x', '1', x'', 1, 2", 0)),
        ] {
            let written = sqlx::raw_sql(&statement).execute(database.pool()).await;
            assert_eq!(written.is_ok(), accepted, "{statement}");
        }
    }
}

// One transaction looked up by identity, with the checkpoint of the same
// snapshot.

impl TransactionStore for Repository {
    fn locate<'a>(
        &'a self,
        transaction: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Location, IndexError>> {
        Box::pin(async move {
            let network = Self::network(&transaction.network)?;
            let (mut read, checkpoint) = self.database.snapshot(&network).await?;
            let held =
                models::Transaction::get_by_id(&mut read, &network, transaction.id.as_str()).await;
            let block = found(held, "failed to read a transaction")?
                .map(models::Transaction::block)
                .transpose()?;
            release(read).await?;
            Ok(Location { checkpoint, block })
        })
    }
}

// Pages of one address's live outputs, read from one snapshot together with
// the observations that say whether the page may be trusted at all.

impl OutputStore for Repository {
    /// One checkpoint-consistent page of live outputs.
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            request.validate()?;
            let network = Self::network(&request.network)?;
            let address = request.address.value.as_str();
            let (mut read, checkpoint) = self.database.snapshot(&network).await?;
            // Refused from this snapshot rather than by whoever reads the page:
            // the observations and the rows are one transaction, so the gate
            // and the rows it guards cannot be read at two different lines.
            let observations = Wallet::find_observed_by_address(&mut read, &network, address)
                .await?
                .into_iter()
                .map(WatchProgress::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            Inspection::new(&observations, checkpoint.as_ref()).spendable()?;
            let after = match &request.after {
                // The live set is not append-only -- a spend removes a row -- so
                // a cursor is still bound to the checkpoint it was issued at.
                Some(cursor) if cursor.checkpoint != checkpoint => {
                    return Err(conflict("outputs changed during pagination"));
                }
                Some(cursor) => Output::resumed(&cursor.position)?,
                None => (String::new(), -1),
            };
            let probe = Probe::new(request.limit);
            let live = Output::find_live(&mut read, &network, address, &after, probe.rows())
                .await?
                .into_iter()
                .map(IndexedOutput::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            release(read).await?;
            let (outputs, next) = probe.page(live, |output| OutputCursor {
                checkpoint: checkpoint.clone(),
                position: Output::position(&output.key()),
            });
            Ok(OutputPage {
                checkpoint,
                outputs,
                next,
            })
        })
    }
}

// The application's half of the `wallet` table ([`WalletRegistry`]): filing a
// wallet with its label and key, and reading every row back, archived ones
// included. Scanning never reads label or key.

impl WalletRegistry for Repository {
    /// One transaction: the pair and the identity are read and the row filed
    /// inside it, under the rules of [`WalletRegistry::register`]; the row is
    /// returned as stored.
    fn register<'a>(
        &'a self,
        registration: Registration,
    ) -> BoxFuture<'a, Result<crate::Wallet, IndexError>> {
        Box::pin(async move {
            registration.validate()?;
            let network = Self::network(&registration.address.network)?;
            let row = models::Wallet::from(registration);
            let mut write = self.database.write().await?;
            let held = models::Wallet::get_by_subject(&mut write, &network, row.subject()).await;
            let filed = match found(held, "failed to read an observation")? {
                Some(held) if !held.archived() && held.id() != row.id() => {
                    return Err(redefinition());
                }
                Some(held) => {
                    let filed = held.merged(row)?;
                    models::Wallet::update_registration(&mut write, &filed).await?;
                    filed
                }
                None => {
                    // The identity is unique across the file, whatever network
                    // holds it.
                    let named = models::Wallet::get_by_id(&mut write, row.id()).await;
                    if found(named, "failed to read a wallet")?.is_some() {
                        return Err(redefinition());
                    }
                    models::Wallet::insert(&mut write, &row).await?;
                    row
                }
            };
            commit(write).await?;
            filed.try_into()
        })
    }

    fn wallet<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<crate::Wallet>, IndexError>> {
        Box::pin(async move {
            let mut connection = self.database.connection().await?;
            let row = models::Wallet::get_by_id(&mut connection, id).await;
            found(row, "failed to read a wallet")?
                .map(TryInto::try_into)
                .transpose()
        })
    }

    fn wallets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<crate::Wallet>, IndexError>> {
        Box::pin(async move {
            let mut connection = self.database.connection().await?;
            models::Wallet::find_all(&mut connection)
                .await?
                .into_iter()
                .map(TryInto::try_into)
                .collect()
        })
    }
}
