//! The file's one schema (`schema.sql`) and the identity that says it holds
//! it.
//!
//! There are no migrations. A file is this store's when its
//! `PRAGMA application_id` is [`APPLICATION_ID`] and its `PRAGMA user_version`
//! is the fingerprint of the exact schema text it was created with — the
//! indexing schema, and the owner schema of an application that keeps its own
//! tables beside it. An empty file is given the schema and stamped; any other
//! file is refused, and a schema change, which changes the fingerprint, needs a
//! fresh file.

use sqlx::{Connection as _, SqliteConnection, SqlitePool};

use super::{commit, failure, store};
use crate::IndexError;

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

    use super::super::{
        Database,
        models::{HistoryEntry, Ledger, Output, Wallet},
    };
    use super::*;
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
                HistoryEntry::AFTER,
                "SEARCH a USING PRIMARY KEY (chain=? AND network=? AND address=? AND \
                 (block_height,transaction_order,transaction_id)>(?,?,?))\n\
                 SEARCH t USING PRIMARY KEY (chain=? AND network=? AND transaction_id=?)",
            ),
            (
                Wallet::OBSERVED,
                "SEARCH wallet USING PRIMARY KEY (chain=? AND network=?)",
            ),
            (
                Ledger::ATTACHED,
                "SEARCH ledger USING PRIMARY KEY (chain=? AND network=? AND address=? AND \
                 asset=? AND block_height>? AND block_height<?)\n\
                 USE TEMP B-TREE FOR LAST 2 TERMS OF ORDER BY",
            ),
            (
                Wallet::OBSERVED_BY_ADDRESS,
                "SEARCH wallet USING INDEX wallet_address (chain=? AND network=? AND address=?)",
            ),
            (
                Wallet::OBSERVED_BY_ID,
                "SEARCH wallet USING INDEX wallet_id (id=?)",
            ),
            (
                Wallet::OBSERVED_BY_SUBJECT,
                "SEARCH wallet USING PRIMARY KEY (chain=? AND network=? AND asset=? AND address=?)",
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
