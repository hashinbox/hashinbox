use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    thread,
};

use redb::{Database, TableDefinition};
use storage::{
    BoxFuture, CommitResult, Error, Key, Namespace, ScanPage, ScanRequest, Store, StoredValue,
    WriteBatch,
};
use tokio::sync::{mpsc, oneshot};

mod format;
mod path;
mod read;
mod support;
mod write;

use path::*;
use support::*;

const DATA_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("data");
const META_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta");
const GLOBAL_VERSION_KEY: &[u8] = b"global-version";
const DATABASE_FORMAT_KEY: &[u8] = b"database-format";
const DATABASE_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Default upper bound for operations accepted but not yet handled by the DB
/// owner thread.
pub const DEFAULT_COMMAND_QUEUE_CAPACITY: usize = 256;

/// A serialized, durable [`Store`] implementation backed by redb.
///
/// All database access is routed through one bounded channel to one dedicated
/// OS thread. This keeps synchronous file I/O off async executors and makes
/// condition evaluation plus the following write one logical critical section.
#[derive(Clone)]
pub struct Redb {
    inner: Arc<WorkerHandle>,
}

struct WorkerHandle {
    command_tx: Option<mpsc::Sender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Redb {
    /// Opens or creates one redb database file using the default command bound.
    ///
    /// The path must be absolute, its parent directory must already exist, and
    /// an existing path must be a compatible redb database file.
    ///
    /// # Errors
    ///
    /// Returns an error when path validation, file locking, physical format
    /// validation, database opening, or owner-thread startup fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_queue_capacity(path, DEFAULT_COMMAND_QUEUE_CAPACITY)
    }

    /// Opens or creates a database with an explicit bounded command capacity.
    ///
    /// # Errors
    ///
    /// A zero capacity is rejected as an invalid request. Other failures are
    /// returned with storage context.
    pub fn open_with_queue_capacity(
        path: impl AsRef<Path>,
        command_queue_capacity: usize,
    ) -> Result<Self, Error> {
        if command_queue_capacity == 0 {
            return Err(Error::invalid_request(
                "redb command queue capacity must be greater than zero",
            ));
        }

        Ok(Self {
            inner: Arc::new(WorkerHandle::open(
                path.as_ref().to_path_buf(),
                command_queue_capacity,
            )?),
        })
    }

    fn command_sender(&self) -> Result<mpsc::Sender<Command>, Error> {
        self.inner
            .command_tx
            .as_ref()
            .cloned()
            .ok_or_else(|| unavailable("redb owner thread is shutting down"))
    }

    #[cfg(test)]
    async fn hold_owner_for_test(&self) -> Result<std_mpsc::Sender<()>, Error> {
        let sender = self.command_sender()?;
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        sender
            .send(Command::TestHold {
                entered: entered_tx,
                release: release_rx,
            })
            .await
            .map_err(|_| unavailable("redb owner thread stopped before test hold was accepted"))?;
        entered_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before entering test hold"))?;
        Ok(release_tx)
    }

    #[cfg(test)]
    async fn fail_after_next_commit_for_test(&self) -> Result<(), Error> {
        let sender = self.command_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::TestFailAfterNextCommit { reply: reply_tx })
            .await
            .map_err(|_| unavailable("redb owner thread stopped before arming commit failure"))?;
        reply_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before commit failure was armed"))
    }

    #[cfg(test)]
    async fn fail_next_reopen_for_test(&self) -> Result<(), Error> {
        let sender = self.command_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::TestFailNextReopen { reply: reply_tx })
            .await
            .map_err(|_| unavailable("redb owner thread stopped before arming reopen failure"))?;
        reply_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before reopen failure was armed"))
    }

    #[cfg(test)]
    async fn reopen_count_for_test(&self) -> Result<usize, Error> {
        let sender = self.command_sender()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .send(Command::TestReopenCount { reply: reply_tx })
            .await
            .map_err(|_| unavailable("redb owner thread stopped before reopen count request"))?;
        reply_rx
            .await
            .map_err(|_| unavailable("redb owner thread stopped before returning the reopen count"))
    }
}

impl Store for Redb {
    fn get<'a>(
        &'a self,
        namespace: &'a Namespace,
        key: &'a Key,
    ) -> BoxFuture<'a, Result<Option<StoredValue>, Error>> {
        let sender = self.command_sender();
        let namespace = namespace.clone();
        let key = key.clone();
        Box::pin(async move {
            let sender = sender?;
            let (reply_tx, reply_rx) = oneshot::channel();
            sender
                .send(Command::Get {
                    namespace,
                    key,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| unavailable("redb owner thread stopped before get was accepted"))?;
            reply_rx.await.map_err(|_| {
                unavailable("redb owner thread stopped before get returned a result")
            })?
        })
    }

    fn scan<'a>(&'a self, request: ScanRequest) -> BoxFuture<'a, Result<ScanPage, Error>> {
        let sender = self.command_sender();
        Box::pin(async move {
            let sender = sender?;
            let (reply_tx, reply_rx) = oneshot::channel();
            sender
                .send(Command::Scan {
                    request,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| unavailable("redb owner thread stopped before scan was accepted"))?;
            reply_rx.await.map_err(|_| {
                unavailable("redb owner thread stopped before scan returned a result")
            })?
        })
    }

    fn commit<'a>(&'a self, batch: WriteBatch) -> BoxFuture<'a, Result<CommitResult, Error>> {
        let sender = self.command_sender();
        Box::pin(async move {
            let sender = sender?;
            let (reply_tx, reply_rx) = oneshot::channel();
            sender
                .send(Command::Commit {
                    batch,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| unavailable("redb owner thread stopped before commit was accepted"))?;
            reply_rx.await.map_err(|_| {
                unavailable("redb owner thread stopped before commit returned a result")
            })?
        })
    }
}

impl WorkerHandle {
    fn open(path: PathBuf, command_queue_capacity: usize) -> Result<Self, Error> {
        let (command_tx, command_rx) = mpsc::channel(command_queue_capacity);
        let (startup_tx, startup_rx) = std_mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("storage-redb-owner".to_owned())
            .spawn(move || {
                let backend = match Backend::open(&path) {
                    Ok(backend) => backend,
                    Err(error) => {
                        drop(startup_tx.send(Err(error)));
                        return;
                    }
                };
                if startup_tx.send(Ok(())).is_ok() {
                    backend.run(command_rx);
                }
            })
            .map_err(|error| unavailable(format!("failed to spawn redb owner thread: {error}")))?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                command_tx: Some(command_tx),
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                drop(command_tx);
                drop(worker.join());
                Err(error)
            }
            Err(error) => {
                drop(command_tx);
                let detail = if worker.join().is_err() {
                    "the owner thread panicked during startup"
                } else {
                    "the owner thread exited during startup"
                };
                Err(unavailable(format!(
                    "failed to receive redb startup result: {error}; {detail}"
                )))
            }
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Closing the final sender lets the owner finish every accepted command
        // before closing the file. A cold copy after final-handle teardown is
        // therefore ordered after all accepted commits.
        drop(self.command_tx.take());
        if let Some(worker) = self.worker.take() {
            drop(worker.join());
        }
    }
}

enum Command {
    Get {
        namespace: Namespace,
        key: Key,
        reply: oneshot::Sender<Result<Option<StoredValue>, Error>>,
    },
    Scan {
        request: ScanRequest,
        reply: oneshot::Sender<Result<ScanPage, Error>>,
    },
    Commit {
        batch: WriteBatch,
        reply: oneshot::Sender<Result<CommitResult, Error>>,
    },
    #[cfg(test)]
    TestHold {
        entered: oneshot::Sender<()>,
        release: std_mpsc::Receiver<()>,
    },
    #[cfg(test)]
    TestFailAfterNextCommit { reply: oneshot::Sender<()> },
    #[cfg(test)]
    TestFailNextReopen { reply: oneshot::Sender<()> },
    #[cfg(test)]
    TestReopenCount { reply: oneshot::Sender<usize> },
}

struct Backend {
    db: Option<Database>,
    database_path: PathBuf,
    #[cfg(test)]
    fail_after_next_commit: bool,
    #[cfg(test)]
    fail_next_reopen: bool,
    #[cfg(test)]
    reopen_count: usize,
}

impl Backend {
    fn open(path: &Path) -> Result<Self, Error> {
        let database_path = DatabasePath::validate(path)?;
        let db = open_database(&database_path.path, database_path.initialize)?;
        let mut backend = Self {
            db: Some(db),
            database_path: database_path.path,
            #[cfg(test)]
            fail_after_next_commit: false,
            #[cfg(test)]
            fail_next_reopen: false,
            #[cfg(test)]
            reopen_count: 0,
        };

        if database_path.initialize {
            backend.initialize_format()?;
        } else {
            backend.require_format()?;
            backend.global_version()?;
        }
        Ok(backend)
    }

    fn database(&self) -> Result<&Database, Error> {
        self.db
            .as_ref()
            .ok_or_else(|| unavailable("redb database is closed after a storage failure"))
    }

    fn ensure_open(&mut self) -> Result<(), Error> {
        if self.db.is_some() {
            return Ok(());
        }
        self.reopen()
    }

    fn reopen(&mut self) -> Result<(), Error> {
        self.db = None;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_reopen) {
            return Err(unavailable("injected redb database reopen failure"));
        }
        let db = open_database(&self.database_path, false)?;
        self.db = Some(db);
        if let Err(error) = self
            .require_format()
            .and_then(|()| self.global_version().map(drop))
        {
            self.db = None;
            return Err(error);
        }
        #[cfg(test)]
        {
            self.reopen_count += 1;
        }
        Ok(())
    }

    fn run(mut self, mut command_rx: mpsc::Receiver<Command>) {
        while let Some(command) = command_rx.blocking_recv() {
            match command {
                Command::Get {
                    namespace,
                    key,
                    reply,
                } => {
                    drop(reply.send(self.get(&namespace, &key)));
                }
                Command::Scan { request, reply } => {
                    drop(reply.send(self.scan(request)));
                }
                Command::Commit { batch, reply } => {
                    // The write remains accepted even if the requester has
                    // cancelled and its oneshot receiver is already gone.
                    drop(reply.send(self.commit(batch)));
                }
                #[cfg(test)]
                Command::TestHold { entered, release } => {
                    let _ignored = entered.send(());
                    let _ignored = release.recv();
                }
                #[cfg(test)]
                Command::TestFailAfterNextCommit { reply } => {
                    self.fail_after_next_commit = true;
                    let _ignored = reply.send(());
                }
                #[cfg(test)]
                Command::TestFailNextReopen { reply } => {
                    self.fail_next_reopen = true;
                    let _ignored = reply.send(());
                }
                #[cfg(test)]
                Command::TestReopenCount { reply } => {
                    let _ignored = reply.send(self.reopen_count);
                }
            }
        }
    }
}

// shared native redb factory preserves cache and error policy for initial create-or-open and
// recovery open-only without constructing a backend state wrapper
// design-lint: allow unclassified-free-function -- backend-owned storage boundary
fn open_database(path: &Path, initialize: bool) -> Result<Database, Error> {
    let mut builder = Database::builder();
    builder.set_cache_size(DATABASE_CACHE_BYTES);
    let result = if initialize {
        builder.create(path)
    } else {
        builder.open(path)
    };
    result.map_err(database_error)
}

#[cfg(test)]
mod store_test {
    use std::{path::PathBuf, task::Poll};

    use super::*;
    use storage::{Condition, ErrorKind, Operation, Version};
    use tempfile::TempDir;

    fn namespace(name: &str) -> Namespace {
        Namespace(name.to_owned())
    }

    fn key(value: &str) -> Key {
        Key(value.as_bytes().to_vec())
    }

    fn value(value: &str) -> storage::Value {
        storage::Value(value.as_bytes().to_vec())
    }

    fn database_path(directory: &TempDir) -> PathBuf {
        directory.path().join("database.redb")
    }

    fn put(namespace: &Namespace, key: &Key, value: &str) -> Operation {
        Operation::Put {
            namespace: namespace.clone(),
            key: key.clone(),
            value: self::value(value),
        }
    }

    #[tokio::test]
    async fn invalid_scan_bounds_preserve_errors_and_leave_the_store_usable() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        for (limit, after, message) in [
            (0, None, "scan limit must be greater than zero"),
            (usize::MAX, None, "scan limit is too large"),
            (
                1,
                Some(key("other")),
                "scan continuation key does not match the requested prefix",
            ),
        ] {
            let error = storage
                .scan(ScanRequest {
                    namespace: records.clone(),
                    prefix: b"entry/".to_vec(),
                    after,
                    limit,
                })
                .await
                .expect_err("invalid scan bounds");
            assert_eq!(error.kind, ErrorKind::InvalidRequest);
            assert_eq!(error.message, message);
        }
        let committed = storage
            .commit(WriteBatch {
                conditions: Vec::new(),
                operations: vec![put(&records, &key("entry/1"), "stored")],
            })
            .await?;
        assert_eq!(committed.version, Version(1));
        assert_eq!(
            storage
                .get(&records, &key("entry/1"))
                .await?
                .map(|stored| stored.value),
            Some(value("stored"))
        );
        assert_eq!(storage.reopen_count_for_test().await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_after_enqueue_does_not_cancel_the_accepted_commit() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("cancelled-request");
        let record_key = key("accepted-write");
        let release = storage.hold_owner_for_test().await?;
        let mut commit = storage.commit(WriteBatch {
            conditions: Vec::new(),
            operations: vec![put(&records, &record_key, "durable")],
        });

        std::future::poll_fn(|context| match commit.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("owner hold must keep the accepted commit pending"),
        })
        .await;
        drop(commit);
        release
            .send(())
            .map_err(|_| other("failed to release redb owner test hold"))?;

        let stored = storage.get(&records, &record_key).await?;
        assert_eq!(stored.map(|stored| stored.value), Some(value("durable")));
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_commit_successful_reopen_does_not_replay() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        let primary = key("primary");
        storage.fail_after_next_commit_for_test().await?;

        let error = storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "persisted-once")],
            })
            .await
            .expect_err("the injected post-persistence result must be ambiguous");

        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(storage.reopen_count_for_test().await?, 1);
        assert_eq!(
            storage.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("persisted-once"),
                version: Version(1),
            })
        );
        assert_eq!(storage.reopen_count_for_test().await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn read_recovers_after_ambiguous_commit_reopen_failure() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        let primary = key("primary");
        storage.fail_after_next_commit_for_test().await?;
        storage.fail_next_reopen_for_test().await?;

        let error = storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "persisted-once")],
            })
            .await
            .expect_err("the injected post-persistence result must be ambiguous");

        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(storage.reopen_count_for_test().await?, 0);
        assert_eq!(
            storage.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("persisted-once"),
                version: Version(1),
            })
        );
        assert_eq!(storage.reopen_count_for_test().await?, 1);
        let page = storage
            .scan(ScanRequest {
                namespace: records.clone(),
                prefix: b"primary".to_vec(),
                after: None,
                limit: 1,
            })
            .await?;
        assert_eq!(
            page.entries,
            vec![(
                primary.clone(),
                StoredValue {
                    value: value("persisted-once"),
                    version: Version(1),
                }
            )]
        );
        assert_eq!(page.next, None);
        Ok(())
    }

    #[tokio::test]
    async fn put_get_and_paginated_scan_are_ordered() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let observations = namespace("observations");
        let other_namespace = namespace("other");

        let result = storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![
                    put(&observations, &key("tx/a"), "a"),
                    put(&observations, &key("tx/b"), "b"),
                    put(&observations, &key("tx/c"), "c"),
                    put(&other_namespace, &key("tx/a"), "isolated"),
                ],
            })
            .await?;
        assert_eq!(result.version, Version(1));
        assert_eq!(
            storage.get(&observations, &key("tx/b")).await?,
            Some(StoredValue {
                value: value("b"),
                version: Version(1),
            })
        );

        let first = storage
            .scan(ScanRequest {
                namespace: observations.clone(),
                prefix: b"tx/".to_vec(),
                after: None,
                limit: 2,
            })
            .await?;
        assert_eq!(
            first
                .entries
                .iter()
                .map(|(entry_key, _)| entry_key.clone())
                .collect::<Vec<_>>(),
            vec![key("tx/a"), key("tx/b")]
        );
        assert_eq!(first.next, Some(key("tx/b")));

        let second = storage
            .scan(ScanRequest {
                namespace: observations,
                prefix: b"tx/".to_vec(),
                after: first.next,
                limit: 2,
            })
            .await?;
        assert_eq!(
            second
                .entries
                .iter()
                .map(|(entry_key, _)| entry_key.clone())
                .collect::<Vec<_>>(),
            vec![key("tx/c")]
        );
        assert_eq!(second.next, None);
        Ok(())
    }

    #[tokio::test]
    async fn stale_condition_rejects_the_complete_batch() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        let primary = key("primary");
        let side_effect = key("side-effect");

        storage
            .commit(WriteBatch {
                conditions: vec![Condition::Missing {
                    namespace: records.clone(),
                    key: primary.clone(),
                }],
                operations: vec![put(&records, &primary, "v1")],
            })
            .await?;
        storage
            .commit(WriteBatch {
                conditions: vec![Condition::Version {
                    namespace: records.clone(),
                    key: primary.clone(),
                    expected: Version(1),
                }],
                operations: vec![put(&records, &primary, "v2")],
            })
            .await?;

        let error = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Version {
                    namespace: records.clone(),
                    key: primary.clone(),
                    expected: Version(1),
                }],
                operations: vec![
                    put(&records, &primary, "stale"),
                    put(&records, &side_effect, "must-not-commit"),
                ],
            })
            .await
            .expect_err("a stale compare-and-swap must fail");
        assert_eq!(error.kind, ErrorKind::Conflict);
        assert_eq!(storage.get(&records, &side_effect).await?, None);
        assert_eq!(
            storage.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("v2"),
                version: Version(2),
            })
        );

        let next = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Missing {
                    namespace: records.clone(),
                    key: side_effect.clone(),
                }],
                operations: vec![put(&records, &side_effect, "committed")],
            })
            .await?;
        assert_eq!(next.version, Version(3));
        Ok(())
    }

    #[tokio::test]
    async fn absent_or_present_condition_conflicts_preserve_batch_and_version() -> Result<(), Error>
    {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        let other_namespace = namespace("other");
        let primary = key("primary");
        let missing = key("missing");
        let side_effect = key("side-effect");
        storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "original")],
            })
            .await?;

        for (condition, message) in [
            (
                Condition::Missing {
                    namespace: records.clone(),
                    key: primary.clone(),
                },
                "missing condition failed in namespace `records` because the key exists",
            ),
            (
                Condition::Version {
                    namespace: records.clone(),
                    key: missing.clone(),
                    expected: Version(2),
                },
                "version condition failed in namespace `records` because the key is missing",
            ),
        ] {
            let batch = WriteBatch {
                conditions: vec![condition],
                operations: vec![
                    Operation::Delete {
                        namespace: records.clone(),
                        key: primary.clone(),
                    },
                    put(&records, &missing, "must-not-commit"),
                    put(&other_namespace, &side_effect, "must-not-commit"),
                ],
            };
            let error = storage
                .commit(batch)
                .await
                .expect_err("conditions must inspect the state before any batch operation");

            assert_eq!(error.kind, ErrorKind::Conflict);
            assert_eq!(error.message, message);
            assert_eq!(storage.get(&records, &missing).await?, None);
            assert_eq!(storage.get(&other_namespace, &side_effect).await?, None);
            assert_eq!(
                storage.get(&records, &primary).await?,
                Some(StoredValue {
                    value: value("original"),
                    version: Version(1),
                })
            );
        }

        let next = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Missing {
                    namespace: records.clone(),
                    key: missing.clone(),
                }],
                operations: vec![put(&records, &missing, "committed")],
            })
            .await?;
        assert_eq!(next.version, Version(2));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_compare_and_swap_has_one_winner() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        let primary = key("primary");
        storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "v1")],
            })
            .await?;

        let first = storage.clone();
        let second = storage.clone();
        let [first_batch, second_batch] = ["first", "second"].map(|replacement| WriteBatch {
            conditions: vec![Condition::Version {
                namespace: records.clone(),
                key: primary.clone(),
                expected: Version(1),
            }],
            operations: vec![put(&records, &primary, replacement)],
        });

        let (first_result, second_result) =
            tokio::join!(first.commit(first_batch), second.commit(second_batch));
        let successes = usize::from(first_result.is_ok()) + usize::from(second_result.is_ok());
        let conflicts = usize::from(matches!(
            first_result,
            Err(Error {
                kind: ErrorKind::Conflict,
                ..
            })
        )) + usize::from(matches!(
            second_result,
            Err(Error {
                kind: ErrorKind::Conflict,
                ..
            })
        ));
        assert_eq!(successes, 1);
        assert_eq!(conflicts, 1);
        Ok(())
    }

    #[tokio::test]
    async fn mixed_operations_preserve_input_order_and_one_commit_version() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        let primary = key("primary");
        let removed = key("removed");
        let result = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Missing {
                    namespace: records.clone(),
                    key: primary.clone(),
                }],
                operations: vec![
                    put(&records, &primary, "first"),
                    put(&records, &primary, "replaced"),
                    Operation::Delete {
                        namespace: records.clone(),
                        key: primary.clone(),
                    },
                    put(&records, &primary, "last"),
                    put(&records, &removed, "temporary"),
                    Operation::Delete {
                        namespace: records.clone(),
                        key: removed.clone(),
                    },
                ],
            })
            .await?;

        assert_eq!(result.version, Version(1));
        assert_eq!(
            storage.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("last"),
                version: Version(1),
            })
        );
        assert_eq!(storage.get(&records, &removed).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn delete_removes_the_value() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let storage = Redb::open(database_path(&directory))?;
        let records = namespace("records");
        let primary = key("primary");
        storage
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &primary, "v1")],
            })
            .await?;

        let result = storage
            .commit(WriteBatch {
                conditions: vec![Condition::Version {
                    namespace: records.clone(),
                    key: primary.clone(),
                    expected: Version(1),
                }],
                operations: vec![Operation::Delete {
                    namespace: records.clone(),
                    key: primary.clone(),
                }],
            })
            .await?;

        assert_eq!(result.version, Version(2));
        assert_eq!(storage.get(&records, &primary).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn persisted_values_and_global_version_survive_reopen() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database = database_path(&directory);
        let records = namespace("records");
        let primary = key("primary");
        {
            let storage = Redb::open(&database)?;
            storage
                .commit(WriteBatch {
                    conditions: vec![],
                    operations: vec![put(&records, &primary, "v1")],
                })
                .await?;
        }

        let reopened = Redb::open(&database)?;
        assert_eq!(
            reopened.get(&records, &primary).await?,
            Some(StoredValue {
                value: value("v1"),
                version: Version(1),
            })
        );
        let result = reopened
            .commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(&records, &key("second"), "v2")],
            })
            .await?;
        assert_eq!(result.version, Version(2));
        Ok(())
    }
}

#[cfg(test)]
mod recovery_test {
    use std::{fs, path::Path, process::Command};

    use super::*;
    use redb::{Durability, TableDefinition};
    use storage::{ErrorKind, Operation, Version};
    use tempfile::TempDir;

    use crate::codec::{GlobalVersion, StoredRecord, encode_physical_key};

    const CRASH_CHILD_PATH_ENV: &str = "STORAGE_REDB_CRASH_CHILD_PATH";

    fn namespace(name: &str) -> Namespace {
        Namespace(name.to_owned())
    }

    fn key(value: &str) -> Key {
        Key(value.as_bytes().to_vec())
    }

    fn value(value: &str) -> storage::Value {
        storage::Value(value.as_bytes().to_vec())
    }

    fn put(namespace: &Namespace, key: &Key, value: &str) -> Operation {
        Operation::Put {
            namespace: namespace.clone(),
            key: key.clone(),
            value: self::value(value),
        }
    }

    fn create_raw(path: &Path) -> Result<Database, Error> {
        Database::create(path)
            .map_err(|error| other(format!("test database create failed: {error}")))
    }

    fn open_raw(path: &Path) -> Result<Database, Error> {
        Database::open(path).map_err(|error| other(format!("test database open failed: {error}")))
    }

    #[tokio::test]
    async fn closed_database_file_can_be_copied_and_reopened() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        let backup_path = directory.path().join("backup.redb");
        let records = namespace("records");
        let snapshot_key = key("snapshot");
        {
            let storage = Redb::open(&database_path)?;
            storage
                .commit(WriteBatch {
                    conditions: vec![],
                    operations: vec![put(&records, &snapshot_key, "captured")],
                })
                .await?;
        }

        let copied = fs::copy(&database_path, &backup_path)
            .map_err(|error| other(format!("cold database copy failed: {error}")))?;
        assert!(copied > 0);
        let backup = Redb::open(&backup_path)?;
        assert_eq!(
            backup.get(&records, &snapshot_key).await?,
            Some(StoredValue {
                value: value("captured"),
                version: Version(1),
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn second_open_is_rejected_by_file_lock() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        let storage = Redb::open(&database_path)?;

        let second_open = match Redb::open(&database_path) {
            Ok(_) => panic!("redb must enforce one writable owner per file"),
            Err(error) => error,
        };
        assert_eq!(second_open.kind, ErrorKind::Unavailable);

        drop(storage);
        let _reopened = Redb::open(&database_path)?;
        Ok(())
    }

    #[tokio::test]
    async fn populated_database_without_format_marker_is_rejected() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        let records = namespace("records");
        let primary = key("primary");
        {
            let db = create_raw(&database_path)?;
            let mut transaction = db
                .begin_write()
                .map_err(|error| other(format!("test transaction failed: {error}")))?;
            transaction
                .set_durability(Durability::Immediate)
                .map_err(|error| other(format!("test durability failed: {error}")))?;
            {
                let mut data = transaction
                    .open_table(DATA_TABLE)
                    .map_err(|error| other(format!("test data table failed: {error}")))?;
                let mut meta = transaction
                    .open_table(META_TABLE)
                    .map_err(|error| other(format!("test metadata table failed: {error}")))?;
                let physical_key = encode_physical_key(&records, &primary)?;
                let stored = StoredRecord::new(value("stored-value"), Version(1))?.encode()?;
                drop(
                    data.insert(physical_key.as_slice(), stored.as_slice())
                        .map_err(|error| other(format!("test data write failed: {error}")))?,
                );
                let version = GlobalVersion::new(Version(1))?.encode()?;
                drop(
                    meta.insert(GLOBAL_VERSION_KEY, version.as_slice())
                        .map_err(|error| other(format!("test metadata write failed: {error}")))?,
                );
            }
            transaction
                .commit()
                .map_err(|error| other(format!("test commit failed: {error}")))?;
        }

        let open_error = match Redb::open(&database_path) {
            Ok(_) => panic!("a populated database without a format marker must fail closed"),
            Err(error) => error,
        };
        assert_eq!(open_error.kind, ErrorKind::CorruptData);
        Ok(())
    }

    #[test]
    fn populated_database_without_global_version_is_rejected() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        let records = namespace("records");
        let primary = key("primary");
        {
            let db = create_raw(&database_path)?;
            let mut transaction = db
                .begin_write()
                .map_err(|error| other(format!("test transaction failed: {error}")))?;
            transaction
                .set_durability(Durability::Immediate)
                .map_err(|error| other(format!("test durability failed: {error}")))?;
            {
                let mut data = transaction
                    .open_table(DATA_TABLE)
                    .map_err(|error| other(format!("test data table failed: {error}")))?;
                let mut meta = transaction
                    .open_table(META_TABLE)
                    .map_err(|error| other(format!("test metadata table failed: {error}")))?;
                let physical_key = encode_physical_key(&records, &primary)?;
                let stored = StoredRecord::new(value("stored-value"), Version(1))?.encode()?;
                drop(
                    data.insert(physical_key.as_slice(), stored.as_slice())
                        .map_err(|error| other(format!("test data write failed: {error}")))?,
                );
                drop(
                    meta.insert(DATABASE_FORMAT_KEY, super::format::DATABASE_FORMAT)
                        .map_err(|error| other(format!("test marker write failed: {error}")))?,
                );
            }
            transaction
                .commit()
                .map_err(|error| other(format!("test commit failed: {error}")))?;
        }

        let error = match Redb::open(&database_path) {
            Ok(_) => panic!("populated data without a global version must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::CorruptData);
        Ok(())
    }

    #[test]
    fn empty_initialized_database_without_global_version_reopens() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        drop(Redb::open(&database_path)?);

        let reopened = Redb::open(&database_path)?;
        drop(reopened);
        Ok(())
    }

    #[test]
    fn database_with_only_a_foreign_table_is_rejected() -> Result<(), Error> {
        const FOREIGN_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("foreign");
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        {
            let db = create_raw(&database_path)?;
            let transaction = db
                .begin_write()
                .map_err(|error| other(format!("test transaction failed: {error}")))?;
            {
                let mut foreign = transaction
                    .open_table(FOREIGN_TABLE)
                    .map_err(|error| other(format!("test foreign table failed: {error}")))?;
                drop(
                    foreign
                        .insert(b"key".as_slice(), b"value".as_slice())
                        .map_err(|error| other(format!("test foreign write failed: {error}")))?,
                );
            }
            transaction
                .commit()
                .map_err(|error| other(format!("test commit failed: {error}")))?;
        }

        let error = match Redb::open(&database_path) {
            Ok(_) => panic!("a redb file without this adapter's tables must not be adopted"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::CorruptData, "{error}");
        Ok(())
    }

    #[test]
    fn incompatible_table_type_is_rejected() -> Result<(), Error> {
        const WRONG_DATA_TABLE: TableDefinition<u64, u64> = TableDefinition::new("data");
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        {
            let db = create_raw(&database_path)?;
            let transaction = db
                .begin_write()
                .map_err(|error| other(format!("test transaction failed: {error}")))?;
            {
                let mut data = transaction
                    .open_table(WRONG_DATA_TABLE)
                    .map_err(|error| other(format!("test wrong table failed: {error}")))?;
                drop(
                    data.insert(1, 2)
                        .map_err(|error| other(format!("test wrong write failed: {error}")))?,
                );
                let mut meta = transaction
                    .open_table(META_TABLE)
                    .map_err(|error| other(format!("test metadata table failed: {error}")))?;
                drop(
                    meta.insert(DATABASE_FORMAT_KEY, super::format::DATABASE_FORMAT)
                        .map_err(|error| other(format!("test marker write failed: {error}")))?,
                );
            }
            transaction
                .commit()
                .map_err(|error| other(format!("test commit failed: {error}")))?;
        }

        let error = match Redb::open(&database_path) {
            Ok(_) => panic!("an incompatible data table type must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::CorruptData);
        Ok(())
    }

    #[test]
    fn different_database_format_fails_closed() -> Result<(), Error> {
        for marker in [
            b"".as_slice(),
            b"w3-storage-red".as_slice(),
            b"w3-storage-redb-extra".as_slice(),
            b"another-database-format".as_slice(),
        ] {
            let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
            let database_path = directory.path().join("database.redb");
            {
                let storage = Redb::open(&database_path)?;
                drop(storage);
            }
            {
                let db = open_raw(&database_path)?;
                let transaction = db
                    .begin_write()
                    .map_err(|error| other(format!("test transaction failed: {error}")))?;
                {
                    let mut meta = transaction
                        .open_table(META_TABLE)
                        .map_err(|error| other(format!("test metadata table failed: {error}")))?;
                    let previous = meta
                        .insert(DATABASE_FORMAT_KEY, marker)
                        .map_err(|error| other(format!("test marker write failed: {error}")))?;
                    drop(previous);
                }
                transaction
                    .commit()
                    .map_err(|error| other(format!("test commit failed: {error}")))?;
            }

            let error = match Redb::open(&database_path) {
                Ok(_) => panic!("a different database format must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.kind, ErrorKind::CorruptData);
            assert_eq!(
                error.message,
                "database format is not supported by this adapter"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn malformed_persisted_frame_is_reported_as_corruption() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        let records = namespace("records");
        let primary = key("primary");
        {
            let storage = Redb::open(&database_path)?;
            drop(storage);
        }
        {
            let db = open_raw(&database_path)?;
            let transaction = db
                .begin_write()
                .map_err(|error| other(format!("test transaction failed: {error}")))?;
            {
                let mut data = transaction
                    .open_table(DATA_TABLE)
                    .map_err(|error| other(format!("test data table failed: {error}")))?;
                let mut meta = transaction
                    .open_table(META_TABLE)
                    .map_err(|error| other(format!("test metadata table failed: {error}")))?;
                let physical_key = encode_physical_key(&records, &primary)?;
                drop(
                    data.insert(physical_key.as_slice(), b"invalid".as_slice())
                        .map_err(|error| other(format!("test malformed write failed: {error}")))?,
                );
                let version = GlobalVersion::new(Version(1))?.encode()?;
                drop(
                    meta.insert(GLOBAL_VERSION_KEY, version.as_slice())
                        .map_err(|error| other(format!("test metadata write failed: {error}")))?,
                );
            }
            transaction
                .commit()
                .map_err(|error| other(format!("test commit failed: {error}")))?;
        }

        let reopened = Redb::open(&database_path)?;
        let error = reopened
            .get(&records, &primary)
            .await
            .expect_err("a malformed value frame must be rejected");
        assert_eq!(error.kind, ErrorKind::CorruptData);
        Ok(())
    }

    #[test]
    fn relative_path_is_rejected() {
        let error = match Redb::open("relative-index.redb") {
            Ok(_) => panic!("storage must not resolve ambient process paths"),
            Err(error) => error,
        };

        assert_eq!(error.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn directory_path_is_rejected() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let error = match Redb::open(directory.path()) {
            Ok(_) => panic!("a database directory must not be treated as a redb file"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        Ok(())
    }

    #[test]
    fn missing_parent_directory_is_rejected() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("missing").join("database.redb");
        let error = match Redb::open(database_path) {
            Ok(_) => panic!("the adapter must not create database parent directories"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        Ok(())
    }

    #[test]
    fn foreign_file_is_rejected() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        fs::write(&database_path, b"this is not a redb database")
            .map_err(|error| other(format!("test foreign file write failed: {error}")))?;

        let error = match Redb::open(&database_path) {
            Ok(_) => panic!("an arbitrary existing file must not be adopted"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::CorruptData, "{error}");
        Ok(())
    }

    #[test]
    fn zero_queue_capacity_is_rejected() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");

        let error = match Redb::open_with_queue_capacity(database_path, 0) {
            Ok(_) => panic!("a zero-capacity queue cannot provide bounded command delivery"),
            Err(error) => error,
        };

        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        Ok(())
    }

    #[tokio::test]
    async fn forced_process_exit_preserves_immediate_commit() -> Result<(), Error> {
        let directory = TempDir::new().map_err(|error| other(error.to_string()))?;
        let database_path = directory.path().join("database.redb");
        let output = Command::new(
            std::env::current_exe()
                .map_err(|error| other(format!("test executable failed: {error}")))?,
        )
        .arg("--exact")
        .arg("backend::recovery_test::forced_exit_writer_helper")
        .arg("--nocapture")
        .env(CRASH_CHILD_PATH_ENV, &database_path)
        .output()
        .map_err(|error| other(format!("failed to start forced-exit child: {error}")))?;
        if !output.status.success() {
            return Err(other(format!(
                "forced-exit child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let storage = Redb::open(&database_path)?;
        assert_eq!(
            storage
                .get(&namespace("crash-recovery"), &key("committed"))
                .await?,
            Some(StoredValue {
                value: value("durable"),
                version: Version(1),
            })
        );
        Ok(())
    }

    #[test]
    fn forced_exit_writer_helper() {
        let Some(path) = std::env::var_os(CRASH_CHILD_PATH_ENV) else {
            return;
        };
        let storage = Redb::open(path).expect("forced-exit child must open redb");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("forced-exit child must create a runtime");
        runtime
            .block_on(storage.commit(WriteBatch {
                conditions: vec![],
                operations: vec![put(
                    &namespace("crash-recovery"),
                    &key("committed"),
                    "durable",
                )],
            }))
            .expect("forced-exit child commit must return");
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use redb::ReadableDatabase;

    use super::*;

    #[test]
    fn zero_capacity_is_rejected_before_path_validation() {
        for path in ["", "relative.redb"] {
            let error = Redb::open_with_queue_capacity(path, 0)
                .err()
                .expect("invalid queue capacity");
            assert_eq!(error.kind, storage::ErrorKind::InvalidRequest);
            assert_eq!(
                error.message,
                "redb command queue capacity must be greater than zero"
            );
        }
    }

    #[test]
    fn failed_startup_releases_the_file_without_changing_foreign_data() {
        const FOREIGN: TableDefinition<&str, &str> = TableDefinition::new("foreign");
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("foreign.redb");
        let database = Database::create(&path).expect("foreign database");
        let transaction = database.begin_write().expect("foreign transaction");
        {
            let mut table = transaction.open_table(FOREIGN).expect("foreign table");
            table.insert("sentinel", "preserved").expect("foreign row");
        }
        transaction.commit().expect("foreign commit");
        drop(database);

        let error = Redb::open(&path).err().expect("foreign format rejected");
        assert_eq!(error.kind, storage::ErrorKind::CorruptData);
        let reopened = Database::open(&path).expect("startup failure releases file lock");
        let read = reopened.begin_read().expect("read foreign database");
        let table = read.open_table(FOREIGN).expect("foreign table remains");
        assert_eq!(
            table.get("sentinel").expect("foreign row").unwrap().value(),
            "preserved"
        );
    }
}
