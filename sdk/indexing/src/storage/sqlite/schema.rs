//! The file's one schema (`schema/sdk.sql`, at the repository root) and the
//! identity that says it holds it.
//!
//! There are no migrations. A file is this store's when its
//! `PRAGMA application_id` is [`APPLICATION_ID`] and its `PRAGMA user_version`
//! is the fingerprint of the exact schema text it was created with — the
//! indexing schema, and the owner schema of an application that keeps its own
//! tables beside it. An empty file is given the schema and stamped; any other
//! file is refused, and a schema change, which changes the fingerprint, needs a
//! fresh file.

use sqlx::Connection as _;
use sqlx::{SqliteConnection, SqlitePool};

use super::{commit, failure, store};
use crate::IndexError;

/// The SDK's tables and views, applied whole to an empty file.
const SCHEMA: &str = include_str!("../../../../../schema/sdk.sql");

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
