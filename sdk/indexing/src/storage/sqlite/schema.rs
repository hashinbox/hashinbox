//! What the file is, what shape it is in, and how it is brought forward.
//!
//! This is the half `docs/design/030 §8 (ex-031)` section 5.2 calls the one thing the
//! key-value layout it replaced genuinely could not do. A format marker that
//! either matches or does not freezes a stored record's shape for the life of
//! the file; here the file says which application owns it and how many ordered
//! steps have been applied to it, and a step may alter what an earlier step
//! wrote. `docs/SYSTEM_REQUIREMENTS.md` revoked the durable-layout exception on
//! the strength of exactly that.

use rusqlite::{Connection, types::FromSql};

use super::{Database, IndexError, failure, steps::MIGRATIONS, store};

/// `PRAGMA application_id` of every file this adapter creates. A file carrying
/// any other value is some other application's and is refused rather than
/// migrated.
const APPLICATION_ID: i32 = 0x7733_5142;

/// How long a statement waits for another connection's write lock before it
/// gives up. Three chains commit concurrently and each commit is under a
/// millisecond, so this is a ceiling nothing is expected to approach.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl Database {
    /// The durability and concurrency settings this adapter depends on.
    ///
    /// `WAL` is what lets readers run alongside the one writer, and is a
    /// property of the file rather than the connection. `FULL` is what makes a
    /// commit that returned one that has reached the disk. `foreign_keys` is
    /// per-connection and off by default, and `movement` needs it.
    ///
    /// `fullfsync` is what makes `FULL` mean that on macOS. It is a macOS-only
    /// pragma and a no-op elsewhere: SQLite's own default there is a plain
    /// `fsync` the drive may still be holding in cache, so without it a
    /// benchmark measures the disk cache rather than the store.
    pub(super) fn configure(connection: &Connection) -> Result<(), IndexError> {
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| failure(&error, "failed to set the sqlite busy timeout"))?;
        connection
            .execute_batch(
                "PRAGMA synchronous = FULL; PRAGMA fullfsync = ON; PRAGMA foreign_keys = ON;",
            )
            .map_err(|error| failure(&error, "failed to configure sqlite"))?;
        let mode: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|error| failure(&error, "failed to enable the sqlite write-ahead log"))?;
        if mode.eq_ignore_ascii_case("wal") {
            return Ok(());
        }
        Err(store(format!(
            "sqlite refused write-ahead logging and stayed in {mode} mode"
        )))
    }

    /// Applies every schema step the file is behind, each in one transaction
    /// with the `user_version` that records it.
    pub(super) fn migrate(connection: &Connection) -> Result<(), IndexError> {
        let identifier: i32 = Self::setting(connection, "application_id")?;
        let applied = usize::try_from(Self::setting::<i64>(connection, "user_version")?)
            .map_err(|_| store("sqlite file carries a negative schema version"))?;
        if identifier == 0 && applied == 0 {
            // Both pragmas are zero in *any* database nobody stamped, an
            // unrelated application's included, so an unmarked file is adopted
            // only when it is empty. Otherwise this build would stamp somebody
            // else's data as its own and run every step against it, which is
            // exactly the file `AGENTS.md` says to refuse rather than interpret.
            if Self::populated(connection)? {
                return Err(store(
                    "sqlite file carries tables this build did not create",
                ));
            }
            connection
                .execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID};"))
                .map_err(|error| failure(&error, "failed to stamp the sqlite application id"))?;
        } else if identifier != APPLICATION_ID {
            return Err(store("sqlite file belongs to another application"));
        }
        if applied > MIGRATIONS.len() {
            return Err(store("sqlite file carries a schema newer than this build"));
        }
        for (index, step) in MIGRATIONS.iter().enumerate().skip(applied) {
            let version = index + 1;
            connection
                .execute_batch(&format!(
                    "BEGIN; {step} PRAGMA user_version = {version}; COMMIT;"
                ))
                .map_err(|error| failure(&error, "failed to apply a sqlite schema step"))?;
        }
        Ok(())
    }

    /// Whether the file already holds an object of its own.
    ///
    /// The one question that separates a fresh file from an unmarked stranger:
    /// neither carries an application id or a schema version, and only the
    /// first is empty.
    fn populated(connection: &Connection) -> Result<bool, IndexError> {
        connection
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|objects| objects > 0)
            .map_err(|error| failure(&error, "failed to read the sqlite object list"))
    }

    /// One `PRAGMA` this adapter reads as a value.
    pub(super) fn setting<T: FromSql>(
        connection: &Connection,
        name: &str,
    ) -> Result<T, IndexError> {
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .map_err(|error| failure(&error, "failed to read a sqlite pragma"))
    }
}

#[cfg(test)]
mod schema_test {
    use tempfile::TempDir;

    use super::*;

    /// The file says what it is and which schema it carries, so one that is
    /// another application's, or a newer build's, is refused rather than
    /// interpreted. This is the migration story the key-value layout cannot
    /// tell: a number the file carries, rather than a key tag the code must
    /// leave dead forever.
    #[test]
    fn a_foreign_file_and_a_newer_schema_are_both_refused() {
        let directory = TempDir::new().expect("temporary directory");
        let foreign = directory.path().join("foreign.db");
        Connection::open(&foreign)
            .expect("foreign file")
            .execute_batch("PRAGMA application_id = 12345; CREATE TABLE other (value TEXT);")
            .expect("foreign schema");
        let error = Database::open(&foreign)
            .map(drop)
            .expect_err("foreign application");
        assert_eq!(error.message, "sqlite file belongs to another application");

        // An unmarked stranger: both pragmas are zero, as they are in every
        // database nobody stamped, so emptiness is the only thing that tells it
        // apart from a file this build is about to create.
        let unmarked = directory.path().join("unmarked.db");
        Connection::open(&unmarked)
            .expect("unmarked file")
            .execute_batch("CREATE TABLE theirs (value TEXT);")
            .expect("somebody else's table");
        let error = Database::open(&unmarked)
            .map(drop)
            .expect_err("unmarked but populated");
        assert_eq!(
            error.message,
            "sqlite file carries tables this build did not create"
        );

        let ahead = directory.path().join("ahead.db");
        drop(Database::open(&ahead).expect("fresh file"));
        Connection::open(&ahead)
            .expect("reopen")
            .execute_batch("PRAGMA user_version = 99;")
            .expect("bump");
        let error = Database::open(&ahead).map(drop).expect_err("newer schema");
        assert_eq!(
            error.message,
            "sqlite file carries a schema newer than this build"
        );
    }

    /// The three settings a benchmark is only honest with.
    ///
    /// `docs/design/030 §8 (ex-031)` step 2 measured this store as eighty-five times faster
    /// than the one it replaced until it noticed that `synchronous = FULL` is a
    /// plain `fsync` on macOS, which the drive may still be holding in cache.
    /// These are asserted rather than assumed, because a pragma SQLite silently
    /// declines would make every benchmark a measurement of the disk cache.
    #[test]
    fn durability_and_concurrency_settings_are_the_ones_asked_for() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("pragmas.db");
        drop(Database::open(&path).expect("open"));
        let connection = Connection::open(&path).expect("inspect");
        Database::configure(&connection).expect("configure");
        for (pragma, expected) in [("synchronous", 2_i64), ("fullfsync", 1)] {
            assert_eq!(
                Database::setting::<i64>(&connection, pragma).expect("pragma"),
                expected,
                "{pragma}"
            );
        }
        assert_eq!(
            Database::setting::<String>(&connection, "journal_mode").expect("journal"),
            "wal"
        );
    }

    /// Opening the same file twice applies every step once, and a file this
    /// build created is stamped with both its identity and its schema version.
    #[test]
    fn a_reopened_file_is_stamped_and_migrated_once() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("index.db");
        for _ in 0..2 {
            drop(Database::open(&path).expect("open"));
        }
        let connection = Connection::open(&path).expect("inspect");
        assert_eq!(
            Database::setting::<i32>(&connection, "application_id").expect("id"),
            APPLICATION_ID
        );
        assert_eq!(
            Database::setting::<i64>(&connection, "user_version").expect("version"),
            MIGRATIONS.len() as i64
        );
    }
}
