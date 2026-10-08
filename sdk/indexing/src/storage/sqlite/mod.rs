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
mod reads;
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
