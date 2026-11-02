//! The job queue: `tasks::Store` and `tasks::Changes` on the one file. Every
//! statement is the [`Task`] model's; a write is announced after it lands.

use tasks::{Changes, Id, Job, Jobs, Store, StoreError, StoreFuture};
use tokio::sync::broadcast;

use super::Storage;
use crate::models::{Task, task};

/// A store failure, without any bound value.
fn backend(error: anyhow::Error) -> StoreError {
    StoreError::Backend(format!("{error:#}"))
}

fn row(job: &Job) -> Result<Task, StoreError> {
    Task::try_from(job).map_err(|error| StoreError::Invalid(format!("{error:#}")))
}

fn now() -> Result<i64, StoreError> {
    i64::try_from(tasks::now()).map_err(|_| StoreError::Invalid("time is above range".into()))
}

impl Storage {
    /// Tells the queue that jobs changed outside [`Store`] (written with the
    /// run that queued them).
    pub(crate) fn announce(&self) {
        // No subscriber is not an error.
        let _ = self.changes.send(());
    }

    /// Every job a dead process left running back to pending. Run once on
    /// start, before the queue runs.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) async fn recover(&self) -> anyhow::Result<usize> {
        let now = i64::try_from(tasks::now())?;
        self.write(move |connection| Ok(task::db::recover(connection, now)?))
            .await
    }
}

impl Store for Storage {
    fn enqueue(&self, job: Job) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let (row, now) = (row(&job)?, now()?);
            self.write(move |connection| Ok(task::db::insert(connection, &row, now)?))
                .await
                .map_err(backend)?;
            self.announce();
            Ok(())
        })
    }

    fn update(&self, job: Job) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let (row, now) = (row(&job)?, now()?);
            let found = self.write(move |connection| Ok(task::db::update(connection, &row, now)?));
            if !found.await.map_err(backend)? {
                return Err(StoreError::Invalid(format!("job {} is unknown", job.id)));
            }
            self.announce();
            Ok(())
        })
    }

    fn get<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Job>> {
        let id = id.as_str().to_owned();
        Box::pin(async move {
            let row = self.read(move |connection| Ok(task::db::find_by_id(connection, &id)?));
            row.await
                .map_err(backend)?
                .map(|row| Job::try_from(row).map_err(backend))
                .transpose()
        })
    }

    fn next(&self, now: u64) -> StoreFuture<'_, Vec<Job>> {
        Box::pin(async move {
            let rows = self.read(move |connection| Ok(task::db::find_unfinished(connection)?));
            let jobs = rows
                .await
                .map_err(backend)?
                .into_iter()
                .map(|row| Job::try_from(row).map_err(backend))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Jobs::from(jobs)
                .startable(now)
                .into_iter()
                .cloned()
                .collect())
        })
    }
}

impl Changes for Storage {
    fn changes(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, time::Duration};

    use serde_json::json;
    use tasks::{Ack, Retry, Status, Task as Running, Worker};

    use crate::workers::executor::Executor;

    use super::*;

    fn topic(text: &str) -> tasks::Topic {
        tasks::Topic::parse(text).unwrap()
    }

    async fn queue() -> (tempfile::TempDir, Storage) {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("q.db")).unwrap();
        storage.migrate().await.unwrap();
        (directory, storage)
    }

    /// The job `id` once finished, waiting at most five seconds.
    async fn wait(store: &Storage, id: &Id) -> Job {
        let mut changes = Changes::changes(store);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let job = store.get(id).await.unwrap().unwrap();
            if job.status.is_finished() {
                return job;
            }
            let _ = tokio::time::timeout_at(deadline, changes.recv()).await;
            assert!(tokio::time::Instant::now() < deadline, "job did not finish");
        }
    }

    #[tokio::test]
    async fn pruning_deletes_only_jobs_that_succeeded_before() {
        let (_directory, queue) = queue().await;
        let mut ids = Vec::new();
        for status in [Status::Done, Status::Failed, Status::Pending] {
            let job = Job::new(topic("p"), "k", json!({}));
            queue.enqueue(job.clone()).await.unwrap();
            queue
                .update(Job {
                    status,
                    ..job.clone()
                })
                .await
                .unwrap();
            ids.push(job.id);
        }
        let prune = |before: i64| {
            let queue = queue.clone();
            async move {
                queue
                    .write(move |c| {
                        Ok(crate::models::task::db::delete_succeeded_before(
                            c,
                            &["k"],
                            before,
                        )?)
                    })
                    .await
                    .unwrap()
            }
        };
        // Written now: not before the Unix epoch, so nothing goes.
        assert_eq!(prune(0).await, 0);
        assert_eq!(prune(i64::MAX).await, 1);
        assert_eq!(queue.get(&ids[0]).await.unwrap(), None);
        assert!(
            queue.get(&ids[1]).await.unwrap().is_some(),
            "a failure keeps its reason"
        );
        assert!(queue.get(&ids[2]).await.unwrap().is_some());
        let other = Job::new(topic("p"), "other", json!({}));
        queue.enqueue(other.clone()).await.unwrap();
        queue
            .update(Job {
                status: Status::Done,
                ..other.clone()
            })
            .await
            .unwrap();
        assert_eq!(prune(i64::MAX).await, 0, "another kind stays");
    }

    #[tokio::test]
    async fn update_replaces_the_job_whole() {
        let (_directory, queue) = queue().await;
        let mut job = Job::new(topic("a"), "k", json!({"x": 1})).with_deadline(9);
        queue.enqueue(job.clone()).await.unwrap();
        job.state = Some(json!({"z": 3}));
        job.status = Status::Running;
        job.attempts = 1;
        job.reason = Some("why".into());
        queue.update(job.clone()).await.unwrap();
        assert_eq!(queue.get(&job.id).await.unwrap(), Some(job));
        assert_eq!(queue.get(&Id::new()).await.unwrap(), None);
        assert!(
            queue
                .update(Job::new(topic("a"), "k", json!(null)))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn next_never_starts_a_conflicting_topic_or_overtakes() {
        let (_directory, queue) = queue().await;
        // b w1, t1 (the whole network), b w1, b w1, b w2.
        let jobs: Vec<Job> = ["n.w1", "n", "n.w1", "n.w1", "n.w2"]
            .into_iter()
            .map(|text| Job::new(topic(text), "k", json!({})))
            .collect();
        let later = Job::new(topic("m"), "k", json!({})).run_at(10);
        for job in jobs.iter().chain([&later]) {
            queue.enqueue(job.clone()).await.unwrap();
        }
        let next = |now| {
            let queue = queue.clone();
            async move {
                let next = queue.next(now).await.unwrap();
                next.into_iter().map(|job| job.id).collect::<Vec<_>>()
            }
        };
        let finish = |job: &Job| {
            let queue = queue.clone();
            let job = Job {
                status: Status::Done,
                ..job.clone()
            };
            async move { queue.update(job).await.unwrap() }
        };
        assert_eq!(next(5).await, [jobs[0].id.clone()]);
        assert_eq!(next(10).await, [jobs[0].id.clone(), later.id.clone()]);
        // Running holds its topic as much as pending does.
        let running = Job {
            status: Status::Running,
            ..jobs[0].clone()
        };
        queue.update(running).await.unwrap();
        assert!(next(5).await.is_empty());
        finish(&jobs[0]).await;
        assert_eq!(next(5).await, [jobs[1].id.clone()]);
        finish(&jobs[1]).await;
        assert_eq!(next(5).await, [jobs[2].id.clone(), jobs[4].id.clone()]);
        finish(&jobs[2]).await;
        finish(&jobs[4]).await;
        assert_eq!(next(5).await, [jobs[3].id.clone()]);
    }

    #[tokio::test]
    async fn a_wildcard_locks_its_wallet_on_every_network() {
        let (_directory, queue) = queue().await;
        let jobs: Vec<Job> = ["n.w1", "*.w1", "m.w1", "m.w2"]
            .into_iter()
            .map(|text| Job::new(topic(text), "k", json!({})))
            .collect();
        for job in &jobs {
            queue.enqueue(job.clone()).await.unwrap();
        }
        let ids = |next: Vec<Job>| next.into_iter().map(|job| job.id).collect::<Vec<_>>();
        // `*.w1` waits for `n.w1`; `m.w1` waits behind it; `m.w2` is free.
        assert_eq!(
            ids(queue.next(0).await.unwrap()),
            [jobs[0].id.clone(), jobs[3].id.clone()]
        );
    }

    #[tokio::test]
    async fn recover_returns_running_jobs_to_pending() {
        let (_directory, queue) = queue().await;
        let job = Job::new(topic("a"), "k", json!({}));
        queue.enqueue(job.clone()).await.unwrap();
        queue
            .update(Job {
                status: Status::Running,
                ..job.clone()
            })
            .await
            .unwrap();
        assert_eq!(queue.recover().await.unwrap(), 1);
        assert_eq!(
            queue.get(&job.id).await.unwrap().unwrap().status,
            Status::Pending
        );
    }

    /// Retries its first attempt, then saves its state and succeeds.
    struct Saves;

    impl Worker for Saves {
        const KIND: &'static str = "save";
        type Input = serde_json::Value;
        type State = bool;

        async fn run(&self, mut task: Running<serde_json::Value, bool>) -> Ack {
            if task.job().attempts == 1 {
                return task.nack("again");
            }
            match task.save(true).await {
                Ok(()) => task.ack(),
                Err(error) => task.fail(error.to_string()),
            }
        }
    }

    #[tokio::test]
    async fn the_queue_retries_saves_fails_unknown_kinds_and_expires() {
        let (_directory, queue) = queue().await;
        let job = Job::new(topic("a"), "save", json!({"x": 1}));
        let unknown = Job::new(topic("b"), "nope", json!({}));
        let expired = Job::new(topic("c"), "save", json!({})).with_deadline(1);
        for job in [&job, &unknown, &expired] {
            queue.enqueue(job.clone()).await.unwrap();
        }
        let retry = Retry {
            attempts: NonZeroU32::new(2).unwrap(),
            backoff: Duration::from_millis(20),
        };
        let executor = Executor::new(queue.clone()).add(Saves, retry);
        let handle = tokio::spawn(executor.run());
        let done = wait(&queue, &job.id).await;
        assert_eq!((done.status, done.attempts), (Status::Done, 2));
        assert_eq!(
            (done.reason.as_deref(), done.state),
            (Some("again"), Some(json!(true)))
        );
        let failed = wait(&queue, &unknown.id).await;
        assert_eq!(failed.reason.as_deref(), Some("unknown-kind"));
        let failed = wait(&queue, &expired.id).await;
        assert_eq!(failed.reason.as_deref(), Some("expired"));
        handle.abort();
    }
}
