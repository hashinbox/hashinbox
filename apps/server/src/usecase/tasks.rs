//! The job queue: [`Sqlite`], `tasks::Store` on the one file (every statement
//! is the [`Task`] model's), and [`Queue`], it telling each kind's queue
//! when to look again.

use tasks::{Admission, Claim, Id, Job, Notifying, OnLock, Store, StoreError, StoreFuture};

use crate::{
    db::Db,
    models::{Task, int, task},
};

/// The job queue the server runs: the SQLite store, signalling its writes.
pub(crate) type Queue = Notifying<Sqlite>;

/// The queue on `db`.
pub(crate) fn queue(db: Db) -> Queue {
    Notifying::new(Sqlite { db })
}

/// `tasks::Store` on the one SQLite file. Clones share the pool. Every call
/// is one transaction; SQLite's busy timeout absorbs a locked file, so an
/// error is final for that call.
#[derive(Clone)]
pub(crate) struct Sqlite {
    db: Db,
}

/// A store failure, without any bound value.
fn backend(error: anyhow::Error) -> StoreError {
    StoreError::Backend(format!("{error:#}"))
}

/// `value` as a SQLite integer, or the job is not storable.
fn stored(value: u64) -> Result<i64, StoreError> {
    int(value).map_err(|error| StoreError::Invalid(format!("{error:#}")))
}

/// The row of `job`, or the job is not storable.
fn row(job: &Job) -> Result<Task, StoreError> {
    Task::try_from(job).map_err(|error| StoreError::Invalid(format!("{error:#}")))
}

impl Store for Sqlite {
    fn enqueue(&self, job: Job, unique: bool) -> StoreFuture<'_, Admission> {
        Box::pin(async move {
            let (row, now) = (row(&job)?, stored(tasks::now())?);
            let admitted = self.db.write(move |connection| {
                let held = unique
                    .then(|| task::db::find_unfinished(connection, &row.kind, &row.topic))
                    .transpose()?
                    .flatten();
                if let Some(held) = held {
                    return Ok(Admission::Duplicate(Id(u64::try_from(held)?)));
                }
                let id = task::db::insert(connection, &row, now)?;
                Ok(Admission::Queued(Id(u64::try_from(id)?)))
            });
            admitted.await.map_err(backend)
        })
    }

    fn claim(&self, claim: Claim<'_>) -> StoreFuture<'_, Vec<Job>> {
        let kind = claim.kind.to_owned();
        Box::pin(async move {
            let (now, until) = (stored(claim.now)?, stored(claim.until)?);
            let limit = claim.limit;
            let skip = claim.on_lock == OnLock::Skip;
            // A read first: most looks find nothing due, and a write would
            // queue behind every other writer of the file.
            let looked = kind.clone();
            let due = self
                .db
                .read(move |connection| Ok(task::db::any_due(connection, &looked, now)?));
            if !due.await.map_err(backend)? {
                return Ok(Vec::new());
            }
            // In one write, so no job is claimed twice.
            let rows = self.db.write(move |connection| {
                task::db::expire(connection, &kind, now)?;
                let mut ids = Vec::new();
                for (id, blocker) in task::db::due(connection, &kind, now)? {
                    match blocker {
                        Some(holder) if skip => {
                            task::db::skip(connection, id, holder, now)?;
                        }
                        Some(_) => {}
                        None if ids.len() < limit => ids.push(id),
                        None => {}
                    }
                }
                Ok(task::db::lease(connection, &ids, now, until)?)
            });
            rows.await
                .map_err(backend)?
                .into_iter()
                .map(|(id, row)| row.into_job(id).map_err(backend))
                .collect()
        })
    }

    fn update(&self, job: Job, attempt: u32) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let (row, now) = (row(&job)?, stored(tasks::now())?);
            let id = stored(job.id.0)?;
            let attempt = i32::try_from(attempt).map_err(|_| StoreError::Stale)?;
            let written = self
                .db
                .write(move |connection| Ok(task::db::update(connection, id, attempt, &row, now)?));
            if !written.await.map_err(backend)? {
                return Err(StoreError::Stale);
            }
            Ok(())
        })
    }

    fn get(&self, id: Id) -> StoreFuture<'_, Option<Job>> {
        Box::pin(async move {
            let id = stored(id.0)?;
            let row = self
                .db
                .read(move |connection| Ok(task::db::find_by_id(connection, id)?));
            row.await
                .map_err(backend)?
                .map(|(id, row, _, _)| row.into_job(id).map_err(backend))
                .transpose()
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tasks::{Status, Topic, testing::claim};

    use super::*;

    /// The store on a fresh file, and what keeps the file.
    async fn store() -> ((Db, tempfile::TempDir), Sqlite) {
        let (db, directory) = crate::tests::support::db::fresh().await;
        ((db.clone(), directory), Sqlite { db })
    }

    tasks::store_contract!(store);

    #[tokio::test]
    async fn pruning_deletes_only_jobs_that_succeeded_before() {
        let ((db, _directory), store) = store().await;
        let mut ids = Vec::new();
        for status in [
            Status::Done,
            Status::Failed,
            Status::Pending,
            Status::Skipped,
        ] {
            let topic = Topic::parse(&format!("p{}", ids.len())).unwrap();
            let job = Job::new(topic, "k", json!(null)).run_at(0);
            let Admission::Queued(id) = store.enqueue(job, false).await.unwrap() else {
                panic!("a new job");
            };
            let ran = claim(&store, "k", 1, 0).await.remove(0);
            store.update(Job { status, ..ran }, 1).await.unwrap();
            ids.push(id);
        }
        let prune = |before: i64| {
            let db = db.clone();
            async move {
                db.write(move |c| Ok(task::db::delete_done_before(c, &["k"], before)?))
                    .await
                    .unwrap()
            }
        };
        assert_eq!(prune(0).await, 0, "written now, not before the epoch");
        assert_eq!(prune(i64::MAX).await, 2, "succeeded and skipped");
        assert_eq!(store.get(ids[0]).await.unwrap(), None);
        assert!(
            store.get(ids[1]).await.unwrap().is_some(),
            "a failure stays"
        );
        assert!(store.get(ids[2]).await.unwrap().is_some());
    }
}
