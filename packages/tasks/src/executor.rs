//! [`Executor`]: every worker's [`Queue`], started together; a [`Queue`]:
//! one worker's loop claiming its jobs as [`Tasks`]; a [`Task`]: one
//! claimed job and the worker that runs it.

use std::{future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc, time::Duration};

use futures_util::FutureExt as _;
use tokio::{sync::watch, task::JoinSet};
use tracing::Instrument as _;

use crate::{
    job::{Job, millis, now},
    storage::{Changes, Claim, Store, StoreError},
    worker::{Ack, Context, Worker},
};

/// Runs a store's jobs, a loop per worker. A job of a kind with no worker
/// is never claimed.
pub struct Executor<S> {
    store: Arc<S>,
    tick: Duration,
    loops: Vec<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl<S: Store + Changes + 'static> Executor<S> {
    /// No workers, over `store`, looking for due jobs at least every `tick`
    /// (a retry falls due with no write).
    #[must_use]
    pub fn new(store: S, tick: Duration) -> Self {
        Self {
            store: Arc::new(store),
            tick,
            loops: Vec::new(),
        }
    }

    /// Runs jobs of `W::KIND` with `worker`, in a loop of its own.
    #[must_use]
    #[allow(clippy::should_implement_trait, reason = "a builder step, not a sum")]
    pub fn add<W: Worker>(mut self, worker: W) -> Self {
        let queue = Queue::new(worker, self.store.clone(), self.tick);
        self.loops.push(Box::pin(queue.run()));
        self
    }

    /// Starts every worker's queue; they run until the returned
    /// [`Running`] is stopped or dropped.
    ///
    /// # Panics
    /// Outside a Tokio runtime.
    #[must_use]
    pub fn run(self) -> Running {
        let mut loops = JoinSet::new();
        for work in self.loops {
            loops.spawn(work);
        }
        Running(loops)
    }
}

/// A started [`Executor`]. Dropping it stops every loop and the tasks they
/// run; their claims' leases bring the jobs back.
pub struct Running(JoinSet<()>);

impl Running {
    /// Stops every loop and the tasks they run, and waits until they have
    /// stopped.
    pub async fn stop(mut self) {
        self.0.abort_all();
        while self.0.join_next().await.is_some() {}
    }
}

/// The wait before a failed claim is tried again.
const PAUSE: Duration = Duration::from_secs(1);

/// How long a claim's lease outlasts the policy's timeout: time for the
/// run to be dropped and its outcome stored before the job may be claimed
/// again.
const GRACE: Duration = Duration::from_secs(5);

/// One claimed job, the worker that runs it, and the store it is kept in.
pub struct Task<W: Worker> {
    worker: Arc<W>,
    job: Job,
    /// The attempt it was claimed at: what its writes belong to.
    attempt: u32,
    store: Arc<dyn Store>,
}

impl<W: Worker> Task<W> {
    /// `job`, claimed ([`Store::claim`]), run by `worker`, kept in `store`.
    #[must_use]
    pub fn new(worker: Arc<W>, job: Job, store: Arc<dyn Store>) -> Self {
        let attempt = job.attempts;
        Self {
            worker,
            job,
            attempt,
            store,
        }
    }

    /// The job as it is now.
    #[must_use]
    pub const fn job(&self) -> &Job {
        &self.job
    }

    /// Stores the job as it is now ([`Store::update`]).
    ///
    /// # Errors
    /// The store's.
    pub async fn update(&self) -> Result<(), StoreError> {
        self.store.update(self.job.clone(), self.attempt).await
    }

    /// Runs the worker, in a `job` span (`job_id kind topic attempt`) so
    /// every event of the run names its job; logs how it ended as `job.run`
    /// (`ms ack reason`: debug, warn when it failed or retries); and settles
    /// the job by its answer ([`Job::settle`]). Stores nothing.
    pub async fn run(&mut self) {
        let job = &self.job;
        let span = tracing::info_span!(
            "job",
            job_id = job.id.0,
            kind = %job.kind,
            topic = %job.topic,
            attempt = job.attempts
        );
        let started = std::time::Instant::now();
        let ack = match Context::new(&mut self.job, self.store.clone()) {
            Ok(cx) => {
                Self::execute(&self.worker, cx)
                    .instrument(span.clone())
                    .await
            }
            Err(ack) => ack,
        };
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let entered = span.enter();
        let reason = match &ack {
            Ack::Fail(why) | Ack::Retry(why) | Ack::Again(_, Some(why)) => why.as_str(),
            Ack::Done | Ack::Idle | Ack::Again(_, None) => "-",
        };
        if ack.failed() {
            tracing::warn!(target: "job.run", ms, ack = ack.as_str(), reason, "Job ran");
        } else {
            tracing::debug!(target: "job.run", ms, ack = ack.as_str(), reason, "Job ran");
        }
        drop(entered);
        self.job = self.job.clone().settle(ack, &W::POLICY, now());
    }

    /// What `worker` answers `cx`: past the policy's timeout
    /// `Retry("timeout")`, on a panic `Fail("panicked")`.
    async fn execute(worker: &W, cx: Context<'_, W::Input, W::State>) -> Ack {
        let run = AssertUnwindSafe(worker.run(cx)).catch_unwind();
        match tokio::time::timeout(W::POLICY.timeout, run).await {
            Ok(Ok(ack)) => ack,
            Ok(Err(_panic)) => Ack::Fail("panicked".into()),
            Err(_elapsed) => Ack::Retry("timeout".into()),
        }
    }
}

/// Tasks of one worker, not yet running.
pub struct Tasks<W: Worker>(Vec<Task<W>>);

impl<W: Worker> Tasks<W> {
    /// A task per claimed job of `jobs`, run by `worker`, kept in `store`.
    #[must_use]
    pub fn from(worker: &Arc<W>, store: &Arc<dyn Store>, jobs: Vec<Job>) -> Self {
        let tasks = jobs
            .into_iter()
            .map(|job| Task::new(worker.clone(), job, store.clone()));
        Self(tasks.collect())
    }

    /// Runs each task in `running`, storing how it ended; a write that fails
    /// is healed by the claim's lease.
    pub fn spawn(self, running: &mut JoinSet<()>) {
        for mut task in self.0 {
            running.spawn(async move {
                task.run().await;
                let _ = task.update().await;
            });
        }
    }
}

/// One worker's queue: claims its jobs as they may start and runs them.
pub struct Queue<W: Worker> {
    worker: Arc<W>,
    store: Arc<dyn Store>,
    changes: watch::Receiver<()>,
    tick: Duration,
    /// The tasks running, each stored when it ends; dropping the queue
    /// aborts them.
    running: JoinSet<()>,
}

impl<W: Worker> Queue<W> {
    /// `worker`'s queue over `store`, told of changes to `W::KIND`'s jobs,
    /// and looking at least every `tick` (a retry or a lease falls due with
    /// no write).
    pub fn new<S: Store + Changes + 'static>(worker: W, store: Arc<S>, tick: Duration) -> Self {
        Self {
            worker: Arc::new(worker),
            changes: store.changes(W::KIND),
            store,
            tick,
            running: JoinSet::new(),
        }
    }

    /// Runs the worker's [`Worker::before`] until it succeeds; then
    /// forever: claims what fits beside the tasks running, runs it, and
    /// waits for a change, a tick or a task to end.
    pub async fn run(mut self) {
        while self.worker.before(&*self.store).await.is_err() {
            tokio::time::sleep(PAUSE).await;
        }
        let mut tick = tokio::time::interval(self.tick);
        loop {
            match self.claim().await {
                Ok(jobs) => Tasks::from(&self.worker, &self.store, jobs).spawn(&mut self.running),
                Err(_) => tokio::time::sleep(PAUSE).await,
            }
            tokio::select! {
                _ = self.changes.changed() => {}
                _ = tick.tick() => {}
                Some(_) = self.running.join_next() => {}
            }
        }
    }

    /// Claims up to the policy's concurrency, less the tasks running, under
    /// a lease past the policy's timeout by [`GRACE`].
    async fn claim(&self) -> Result<Vec<Job>, StoreError> {
        let now = now();
        let policy = W::POLICY;
        let lease = millis(policy.timeout).saturating_add(millis(GRACE));
        self.store
            .claim(Claim {
                kind: W::KIND,
                limit: policy.concurrency.get().saturating_sub(self.running.len()),
                on_lock: policy.on_lock,
                now,
                until: now.saturating_add(lease),
            })
            .await
    }
}
