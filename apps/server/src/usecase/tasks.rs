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
            let limit = i64::try_from(claim.limit).unwrap_or(i64::MAX);
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
                if skip {
                    task::db::skip_superseded(connection, &kind, now)?;
                }
                let ids = task::db::claimable(connection, &kind, now, limit)?;
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
                .map(|(id, row)| row.into_job(id).map_err(backend))
                .transpose()
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tasks::{Status, Topic};

    use super::*;

    /// The store on a fresh file.
    async fn store() -> (tempfile::TempDir, Db, Sqlite) {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(&directory.path().join("q.db")).unwrap();
        db.migrate().await.unwrap();
        (directory, db.clone(), Sqlite { db })
    }

    /// A job of `kind` on `topic`, due from time 0.
    fn job(kind: &str, topic: &str) -> Job {
        Job::new(Topic::parse(topic).unwrap(), kind, json!(null)).run_at(0)
    }

    async fn enqueue(store: &Sqlite, job: Job) -> Id {
        match store.enqueue(job, false).await.unwrap() {
            Admission::Queued(id) => id,
            Admission::Duplicate(id) => panic!("a duplicate of {id}"),
        }
    }

    /// Claims up to `limit` of `kind` at `now`, leased until `now + 100`.
    async fn claim(store: &Sqlite, kind: &str, limit: usize, on_lock: OnLock, now: u64) -> Vec<Id> {
        let claim = Claim {
            kind,
            limit,
            on_lock,
            now,
            until: now + 100,
        };
        let jobs = store.claim(claim).await.unwrap();
        jobs.into_iter().map(|job| job.id).collect()
    }

    async fn get(store: &Sqlite, id: Id) -> Job {
        store.get(id).await.unwrap().unwrap()
    }

    /// Stores running job `id` done.
    async fn finish(store: &Sqlite, id: Id) {
        let running = get(store, id).await;
        let attempt = running.attempts;
        let done = Job {
            status: Status::Done,
            ..running
        };
        store.update(done, attempt).await.unwrap();
    }

    #[tokio::test]
    async fn enqueue_numbers_jobs_and_unique_keeps_one_unfinished_per_topic() {
        let (_directory, _db, store) = store().await;
        assert_eq!(enqueue(&store, job("k", "a")).await, Id(1));
        let first = store.enqueue(job("u", "n"), true).await.unwrap();
        assert_eq!(first, Admission::Queued(Id(2)));
        let again = store.enqueue(job("u", "n"), true).await.unwrap();
        assert_eq!(again, Admission::Duplicate(Id(2)));
        claim(&store, "u", 1, OnLock::Wait, 0).await;
        let running = store.enqueue(job("u", "n"), true).await.unwrap();
        assert_eq!(running, Admission::Duplicate(Id(2)), "while it runs");
        finish(&store, Id(2)).await;
        let after = store.enqueue(job("u", "n"), true).await.unwrap();
        assert_eq!(after, Admission::Queued(Id(3)));
    }

    #[tokio::test]
    async fn a_claim_follows_line_order_conflicts_and_its_limit() {
        let (_directory, _db, store) = store().await;
        let wallet = enqueue(&store, job("k", "n.w1")).await;
        let network = enqueue(&store, job("k", "n")).await;
        let again = enqueue(&store, job("k", "n.w1")).await;
        let other = enqueue(&store, job("k", "n.w2")).await;
        let elsewhere = enqueue(&store, job("k", "m")).await;
        let later = enqueue(&store, job("k", "x").run_at(50)).await;
        assert_eq!(claim(&store, "k", 1, OnLock::Wait, 0).await, [wallet]);
        assert_eq!(claim(&store, "k", 9, OnLock::Wait, 0).await, [elsewhere]);
        let claimed = get(&store, wallet).await;
        assert_eq!(
            (claimed.status, claimed.attempts, claimed.run_at),
            (Status::Running, 1, 100)
        );
        finish(&store, wallet).await;
        assert_eq!(claim(&store, "k", 9, OnLock::Wait, 0).await, [network]);
        finish(&store, network).await;
        assert_eq!(
            claim(&store, "k", 9, OnLock::Wait, 50).await,
            [again, other, later]
        );
    }

    #[tokio::test]
    async fn a_fresh_run_holds_no_place_until_due_but_a_retry_does() {
        let (_directory, _db, store) = store().await;
        enqueue(&store, job("k", "n").run_at(50)).await;
        let wallet = enqueue(&store, job("k", "n.w1")).await;
        assert_eq!(claim(&store, "k", 9, OnLock::Wait, 0).await, [wallet]);
        let retried = Job {
            attempts: 1,
            ..job("k", "m").run_at(50)
        };
        enqueue(&store, retried).await;
        enqueue(&store, job("k", "m.w1")).await;
        assert!(claim(&store, "k", 9, OnLock::Wait, 0).await.is_empty());
    }

    #[tokio::test]
    async fn a_skip_job_gives_way_to_a_newer_conflicting_job() {
        let (_directory, _db, store) = store().await;
        enqueue(&store, job("send", "n.w1")).await;
        claim(&store, "send", 1, OnLock::Wait, 0).await;
        let round = enqueue(&store, job("round", "n")).await;
        assert!(claim(&store, "round", 1, OnLock::Skip, 0).await.is_empty());
        assert_eq!(get(&store, round).await.status, Status::Pending);
        let newer = enqueue(&store, job("send", "n.w2")).await;
        assert!(claim(&store, "round", 1, OnLock::Skip, 0).await.is_empty());
        let skipped = get(&store, round).await;
        assert_eq!(
            (skipped.status, skipped.reason),
            (Status::Skipped, Some(format!("skipped: {newer}")))
        );
    }

    #[tokio::test]
    async fn a_claim_expires_its_kind_and_reclaims_an_ended_lease() {
        let (_directory, _db, store) = store().await;
        let late = enqueue(&store, job("k", "a").with_deadline(5)).await;
        let leased = enqueue(&store, job("k", "b")).await;
        assert_eq!(claim(&store, "k", 9, OnLock::Wait, 10).await, [leased]);
        let expired = get(&store, late).await;
        assert_eq!(
            (expired.status, expired.reason.as_deref()),
            (Status::Failed, Some("expired"))
        );
        assert!(
            claim(&store, "k", 9, OnLock::Wait, 109).await.is_empty(),
            "leased"
        );
        assert_eq!(claim(&store, "k", 9, OnLock::Wait, 110).await, [leased]);
        assert_eq!(get(&store, leased).await.attempts, 2);
    }

    #[tokio::test]
    async fn an_update_lands_only_while_its_run_holds_the_job() {
        let (_directory, _db, store) = store().await;
        let id = enqueue(&store, job("k", "a")).await;
        let pending = get(&store, id).await;
        assert!(matches!(
            store.update(pending, 0).await,
            Err(StoreError::Stale)
        ));
        claim(&store, "k", 1, OnLock::Wait, 0).await;
        let saved = Job {
            state: Some(json!({"sent": 1})),
            ..get(&store, id).await
        };
        store.update(saved.clone(), 1).await.unwrap();
        assert_eq!(get(&store, id).await.state, Some(json!({"sent": 1})));
        let again = Job {
            status: Status::Pending,
            attempts: 0,
            ..saved.clone()
        };
        store.update(again, 1).await.unwrap();
        assert!(
            matches!(store.update(saved, 1).await, Err(StoreError::Stale)),
            "a late save never undoes how the run ended"
        );
    }

    #[tokio::test]
    async fn pruning_deletes_only_jobs_that_succeeded_before() {
        let (_directory, db, store) = store().await;
        let mut ids = Vec::new();
        for status in [
            Status::Done,
            Status::Failed,
            Status::Pending,
            Status::Skipped,
        ] {
            let id = enqueue(&store, job("k", &format!("p{}", ids.len()))).await;
            claim(&store, "k", 1, OnLock::Wait, 0).await;
            let ran = get(&store, id).await;
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
