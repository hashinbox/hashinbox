//! The job queue: `tasks::Store` on the one file. Every statement is the
//! [`Task`] model's; a set is published after it is written.

use tasks::{Id, Job, State, Store, StoreError, StoreFuture};
use tokio::sync::broadcast;

use super::Storage;
use crate::models::Task;

/// A store failure, without any bound value.
fn backend(error: anyhow::Error) -> StoreError {
    StoreError::Backend(format!("{error:#}"))
}

fn int(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Invalid("time is above the storable range".into()))
}

fn row(job: &Job) -> Result<Task, StoreError> {
    Ok(Task {
        id: job.id.to_string(),
        lane: job.lane.clone(),
        kind: job.kind.clone(),
        state: match job.state {
            State::Pending => 0,
            State::Running => 1,
            State::Done => 2,
            State::Failed => 3,
        },
        payload: job.payload.to_string(),
        run_at: int(job.run_at)?,
    })
}

fn job(row: Task) -> Result<Job, StoreError> {
    let state = match row.state {
        0 => State::Pending,
        1 => State::Running,
        2 => State::Done,
        _ => State::Failed,
    };
    let payload = serde_json::from_str(&row.payload)
        .map_err(|error| StoreError::Backend(format!("stored payload: {error}")))?;
    Ok(Job {
        id: Id::from(row.id),
        lane: row.lane,
        kind: row.kind,
        state,
        payload,
        run_at: u64::try_from(row.run_at).unwrap_or(0),
    })
}

impl Store for Storage {
    fn set(&self, job: Job) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let row = row(&job)?;
            let now = int(tasks::now())?;
            self.write(move |connection| Ok(row.set(connection, now)?))
                .await
                .map_err(backend)?;
            // No subscriber is not an error.
            let _ = self.changes.send(job);
            Ok(())
        })
    }

    fn get<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Job>> {
        let id = id.as_str().to_owned();
        Box::pin(async move {
            let row = self.read(move |connection| Ok(Task::get(connection, &id)?));
            row.await.map_err(backend)?.map(job).transpose()
        })
    }

    fn next(&self, now: u64) -> StoreFuture<'_, Vec<Job>> {
        Box::pin(async move {
            // Above the storable range means "everything".
            let now = i64::try_from(now).unwrap_or(i64::MAX);
            let rows = self.read(move |connection| Ok(Task::next(connection, now)?));
            rows.await.map_err(backend)?.into_iter().map(job).collect()
        })
    }

    fn changes(&self) -> broadcast::Receiver<Job> {
        self.changes.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, num::NonZeroUsize, pin::Pin, sync::Arc, time::Duration};

    use serde_json::json;
    use tasks::{Ack, Executor, Handle, Runner};

    use super::*;

    async fn queue() -> (tempfile::TempDir, Storage) {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("q.db")).unwrap();
        storage.migrate().await.unwrap();
        (directory, storage)
    }

    async fn wait(store: &Arc<dyn Store>, id: &Id) -> Job {
        let handle = Handle::new(id.clone(), store.clone());
        handle.wait(Duration::from_secs(5)).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn set_replaces_the_payload_wholesale() {
        let (_directory, queue) = queue().await;
        let mut job = Job::new("a", "k", json!({"x": 1, "y": 2}));
        queue.set(job.clone()).await.unwrap();
        job.payload = json!({"z": 3});
        job.state = State::Running;
        queue.set(job.clone()).await.unwrap();
        assert_eq!(queue.get(&job.id).await.unwrap(), Some(job));
        assert_eq!(queue.get(&Id::new()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn every_set_is_published() {
        let (_directory, queue) = queue().await;
        let mut changes = queue.changes();
        let mut job = Job::new("a", "k", json!({}));
        queue.set(job.clone()).await.unwrap();
        job.state = State::Done;
        queue.set(job.clone()).await.unwrap();
        assert_eq!(changes.recv().await.unwrap().state, State::Pending);
        assert_eq!(changes.recv().await.unwrap(), job);
    }

    #[tokio::test]
    async fn next_lists_due_unfinished_jobs_by_id() {
        let (_directory, queue) = queue().await;
        let due = Job::new("a", "k", json!({}));
        let mut running = Job::new("b", "k", json!({}));
        running.state = State::Running;
        let mut later = Job::new("a", "k", json!({}));
        later.run_at = 10;
        let mut done = Job::new("c", "k", json!({}));
        done.state = State::Done;
        for job in [&due, &running, &later, &done] {
            queue.set(job.clone()).await.unwrap();
        }
        let ids = |jobs: Vec<Job>| jobs.into_iter().map(|job| job.id).collect::<Vec<_>>();
        let next = queue.next(5).await.unwrap();
        assert_eq!(ids(next), [due.id.clone(), running.id.clone()]);
        let all = queue.next(u64::MAX).await.unwrap();
        assert_eq!(ids(all), [due.id, running.id, later.id]);
    }

    /// Retries its first attempt, then saves `saved` and succeeds.
    struct Saves;

    impl Executor for Saves {
        fn execute(
            &self,
            mut job: Job,
            store: Arc<dyn Store>,
        ) -> Pin<Box<dyn Future<Output = Ack> + Send>> {
            Box::pin(async move {
                if job.payload["attempts"] == 1 {
                    return Ack::Retry("again".into());
                }
                job.payload["saved"] = true.into();
                store
                    .set(job)
                    .await
                    .map_or_else(|error| Ack::Fail(error.to_string()), |()| Ack::Done)
            })
        }
    }

    fn runner(store: &Arc<dyn Store>) -> tokio::task::JoinHandle<()> {
        let runner = Runner::new(store.clone())
            .executor("save", Arc::new(Saves))
            .attempts(NonZeroUsize::new(2).unwrap())
            .backoff(Duration::from_millis(20));
        tokio::spawn(async move { runner.run().await.unwrap() })
    }

    #[tokio::test]
    async fn the_runner_retries_saves_and_fails_unknown_kinds() {
        let (_directory, queue) = queue().await;
        let store: Arc<dyn Store> = Arc::new(queue);
        let running = runner(&store);
        let job = Job::new("a", "save", json!({"x": 1}));
        let unknown = Job::new("b", "nope", json!({}));
        store.set(job.clone()).await.unwrap();
        store.set(unknown.clone()).await.unwrap();
        let done = wait(&store, &job.id).await;
        assert_eq!(done.state, State::Done);
        let expected = json!({"x": 1, "attempts": 2, "reason": "again", "saved": true});
        assert_eq!(done.payload, expected);
        let failed = wait(&store, &unknown.id).await;
        assert_eq!(failed.state, State::Failed);
        assert_eq!(failed.payload["reason"], "unknown-kind");
        running.abort();
    }

    #[tokio::test]
    async fn the_runner_recovers_expires_and_keeps_lane_order() {
        let (_directory, queue) = queue().await;
        let store: Arc<dyn Store> = Arc::new(queue);
        // Left running by a dead process, on its second attempt: it succeeds.
        let mut left = Job::new("a", "save", json!({"attempts": 1}));
        left.state = State::Running;
        let after = Job::new("a", "save", json!({"attempts": 1}));
        let expired = Job::new("b", "save", json!({"deadline": 1}));
        for job in [&left, &after, &expired] {
            store.set(job.clone()).await.unwrap();
        }
        let running = runner(&store);
        let failed = wait(&store, &expired.id).await;
        assert_eq!(failed.payload["reason"], "expired");
        assert_eq!(wait(&store, &after.id).await.state, State::Done);
        let first = wait(&store, &left.id).await;
        assert_eq!(first.state, State::Done);
        running.abort();
    }
}
