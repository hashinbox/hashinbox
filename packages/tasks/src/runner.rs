//! The optional Tokio runner, owner of every queue rule: one running job per
//! lane (oldest first), deadlines, attempts with backoff, recovery, unknown
//! kinds and panics. It wakes on a store change, an ended execution or the
//! earliest `run_at`/deadline. The caller spawns, cancels and joins it.
//!
//! Single process: [`Runner::run`] first returns every running job to pending,
//! so only one process may run jobs from a store.
//!
//! Payload keys the runner owns (the payload must be a JSON object):
//! `attempts` (executions started), `reason` (why it failed or last retried)
//! and, set by the enqueuer, an optional `deadline` (ms): a pending job at or
//! past it fails as `expired`.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use logging::{Level, Logger};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::{
    storage::{Store, StoreError},
    task::{Job, State},
};

/// How an execution ended; the runner stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ack {
    /// Succeeded.
    Done,
    /// Failed for good, with why.
    Fail(String),
    /// Run again after the backoff while attempts remain, with why not now.
    Retry(String),
}

/// Runs the jobs of one kind. A job may run again after a crash: an executor
/// resumes from its payload. To save mid-run (e.g. signed bytes before
/// broadcasting them) it sets the job, still running, with a new payload.
pub trait Executor: Send + Sync {
    /// Executes one running job.
    fn execute(&self, job: Job, store: Arc<dyn Store>)
    -> Pin<Box<dyn Future<Output = Ack> + Send>>;
}

/// Runs jobs over one store.
pub struct Runner {
    store: Arc<dyn Store>,
    executors: HashMap<String, Arc<dyn Executor>>,
    concurrency: NonZeroUsize,
    attempts: u64,
    backoff: Duration,
    logger: Logger,
}

/// The wait after a store failure, and the unit of the idle wait.
const PAUSE: Duration = Duration::from_secs(1);

/// How often a settlement is attempted before the job is left running
/// (its lane blocked until restart).
const SETTLE_TRIES: u32 = 5;

impl Runner {
    /// A runner over `store`: four jobs at once, three attempts, 250 ms
    /// backoff per attempt, no executors, no logging.
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self {
            store,
            executors: HashMap::new(),
            concurrency: NonZeroUsize::new(4).unwrap_or(NonZeroUsize::MIN),
            attempts: 3,
            backoff: Duration::from_millis(250),
            logger: Logger::default(),
        }
    }

    /// Runs jobs of `kind` with `executor`.
    #[must_use]
    pub fn executor(mut self, kind: impl Into<String>, executor: Arc<dyn Executor>) -> Self {
        self.executors.insert(kind.into(), executor);
        self
    }

    /// How many jobs (of different lanes) execute at once.
    #[must_use]
    pub fn concurrency(mut self, concurrency: NonZeroUsize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Executions per job, the first included; a retry past them fails
    /// (`attempts-exhausted: …`).
    #[must_use]
    pub fn attempts(mut self, attempts: NonZeroUsize) -> Self {
        self.attempts = u64::try_from(attempts.get()).unwrap_or(u64::MAX);
        self
    }

    /// A retry after attempt `n` waits `backoff * n`.
    #[must_use]
    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Where events go.
    #[must_use]
    pub fn logger(mut self, logger: Logger) -> Self {
        self.logger = logger;
        self
    }

    /// Returns jobs a dead process left running to pending, then runs jobs
    /// until the caller drops or aborts it. Dropping it aborts the executions
    /// in flight; their jobs stay running until the next start.
    ///
    /// # Errors
    /// The store's failure to recover; later failures are logged and retried.
    pub async fn run(&self) -> Result<(), StoreError> {
        // Subscribe first: a change between a look and the wait is not lost.
        let mut changes = self.store.changes();
        for mut job in self.store.next(u64::MAX).await? {
            if job.state == State::Running {
                job.state = State::Pending;
                self.store.set(job).await?;
            }
        }
        let mut running = JoinSet::new();
        loop {
            let wake = match self.fill(&mut running).await {
                Ok(wake) => wake,
                Err(error) => {
                    self.warn("task.store", "queue store failed", &error.to_string());
                    PAUSE
                }
            };
            tokio::select! {
                _ = changes.recv() => {}
                Some(_) = running.join_next(), if !running.is_empty() => {}
                () = tokio::time::sleep(wake) => {}
            }
        }
    }

    /// Starts every due lane head while there is room; how long until the
    /// earliest waiting head falls due or expires.
    async fn fill(&self, running: &mut JoinSet<()>) -> Result<Duration, StoreError> {
        let now = now();
        let mut wake = PAUSE * 60;
        let mut seen = HashSet::new();
        // Every unfinished job, by id: the first of each lane is its head.
        for mut job in self.store.next(u64::MAX).await? {
            if !seen.insert(job.lane.clone()) || job.state == State::Running {
                continue;
            }
            let deadline = job.payload.get("deadline").and_then(Value::as_u64);
            if deadline.is_some_and(|deadline| deadline <= now) {
                self.store
                    .set(settled(job, State::Failed, "expired"))
                    .await?;
                continue;
            }
            let due = deadline.map_or(job.run_at, |deadline| deadline.min(job.run_at));
            if job.run_at > now {
                wake = wake.min(Duration::from_millis(due - now));
                continue;
            }
            if running.len() >= self.concurrency.get() {
                continue;
            }
            let Some(payload) = job.payload.as_object_mut() else {
                let job = settled(job, State::Failed, "payload-not-object");
                self.store.set(job).await?;
                continue;
            };
            let attempts = payload.get("attempts").and_then(Value::as_u64).unwrap_or(0);
            payload.insert("attempts".into(), (attempts + 1).into());
            job.state = State::Running;
            self.store.set(job.clone()).await?;
            let execution = Execution {
                store: self.store.clone(),
                executor: self.executors.get(&job.kind).cloned(),
                attempts: self.attempts,
                backoff: self.backoff,
                logger: self.logger.clone(),
            };
            running.spawn(execution.run(job));
        }
        Ok(wake)
    }

    fn warn(&self, code: &str, message: &str, error: &str) {
        self.logger
            .event(Level::Warn, code, message, &[("error", error)]);
    }
}

/// `job` ended or rescheduled as `state`, with `reason` in its payload.
fn settled(mut job: Job, state: State, reason: &str) -> Job {
    if let Some(payload) = job.payload.as_object_mut() {
        payload.insert("reason".into(), reason.into());
    }
    job.state = state;
    job
}

/// One running job's execution and settlement.
struct Execution {
    store: Arc<dyn Store>,
    executor: Option<Arc<dyn Executor>>,
    attempts: u64,
    backoff: Duration,
    logger: Logger,
}

impl Execution {
    async fn run(self, job: Job) {
        let id = job.id.clone();
        let ack = match &self.executor {
            None => Ack::Fail("unknown-kind".into()),
            // Its own task, so a panic is a failure and not a blocked lane.
            Some(executor) => tokio::spawn(executor.execute(job, self.store.clone()))
                .await
                .unwrap_or_else(|_| Ack::Fail("panicked".into())),
        };
        for _ in 0..SETTLE_TRIES {
            // Reread: the executor may have saved a new payload.
            let stored = match self.store.get(&id).await {
                Ok(Some(job)) => self.store.set(self.settle(job, &ack)).await,
                Ok(None) => return,
                Err(error) => Err(error),
            };
            match stored {
                Ok(()) => return,
                Err(error) => self.logger.event(
                    Level::Warn,
                    "task.settle",
                    "settlement not stored",
                    &[("task", id.as_str()), ("error", &error.to_string())],
                ),
            }
            tokio::time::sleep(PAUSE).await;
        }
    }

    fn settle(&self, mut job: Job, ack: &Ack) -> Job {
        match ack {
            Ack::Done => {
                job.state = State::Done;
                job
            }
            Ack::Fail(reason) => settled(job, State::Failed, reason),
            Ack::Retry(reason) => {
                let attempts = job.payload.get("attempts").and_then(Value::as_u64);
                let attempts = attempts.unwrap_or(0);
                if attempts >= self.attempts {
                    let reason = format!("attempts-exhausted: {reason}");
                    return settled(job, State::Failed, &reason);
                }
                let attempts = u32::try_from(attempts).unwrap_or(u32::MAX);
                let wait = millis(self.backoff.saturating_mul(attempts));
                job.run_at = now().saturating_add(wait);
                settled(job, State::Pending, reason)
            }
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, millis)
}
