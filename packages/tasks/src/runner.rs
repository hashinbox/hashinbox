//! The optional Tokio runner: claims due tasks, hands them to executors, and
//! stores each settlement. The caller spawns, cancels and joins it.

use std::{
    collections::HashMap,
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use logging::{Level, Logger};
use tokio::task::JoinSet;

use crate::{
    storage::{Store, StoreError},
    task::{Id, Outcome, Settlement},
};

/// What a claimed task hands its executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// The task.
    pub id: Id,
    /// Its lane.
    pub lane: String,
    /// This attempt, from 1.
    pub attempt: u32,
    /// Its payload.
    pub payload: Vec<u8>,
}

/// The executor's explicit settlement; the runner enforces the consequences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ack {
    /// Terminal success, with the stored output.
    Done(Option<Vec<u8>>),
    /// Terminal refusal; never retried.
    Reject(String),
    /// Terminal uncertainty; never retried.
    Ambiguous(String),
    /// Run again after the backoff while attempts remain.
    Retry(String),
}

/// Runs the tasks of one kind. At-least-once: it must be idempotent.
pub trait Executor: Send + Sync {
    /// Executes one delivery.
    fn execute(&self, delivery: Delivery) -> Pin<Box<dyn Future<Output = Ack> + Send>>;
}

/// Claims and executes tasks over one store.
pub struct Runner {
    store: Arc<dyn Store>,
    executors: HashMap<String, Arc<dyn Executor>>,
    owner: String,
    lease: Duration,
    concurrency: NonZeroUsize,
    logger: Logger,
}

impl Runner {
    /// A runner over `store` leasing as `owner` (unique per worker): 30 s
    /// leases, four tasks at once, no executors, no logging.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, owner: impl Into<String>) -> Self {
        Self {
            store,
            executors: HashMap::new(),
            owner: owner.into(),
            lease: Duration::from_secs(30),
            concurrency: NonZeroUsize::new(4).unwrap_or(NonZeroUsize::MIN),
            logger: Logger::default(),
        }
    }

    /// Runs tasks of `kind` with `executor`.
    #[must_use]
    pub fn executor(mut self, kind: impl Into<String>, executor: Arc<dyn Executor>) -> Self {
        self.executors.insert(kind.into(), executor);
        self
    }

    /// How long one execution may hold its task; it is not settled beyond it.
    #[must_use]
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// How many tasks (of different lanes) execute at once.
    #[must_use]
    pub fn concurrency(mut self, concurrency: NonZeroUsize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Where events go.
    #[must_use]
    pub fn logger(mut self, logger: Logger) -> Self {
        self.logger = logger;
        self
    }

    /// Claims and executes until nothing is claimable; how many it settled
    /// or lost.
    ///
    /// # Errors
    /// The store's failure to claim; executions already started still finish.
    pub async fn pass(&self) -> Result<usize, StoreError> {
        let mut running = JoinSet::new();
        let mut done = 0;
        let mut failure = None;
        loop {
            while failure.is_none() && running.len() < self.concurrency.get() {
                let now = now();
                let lease_until = now.saturating_add(millis(self.lease));
                match self.store.claim(&self.owner, lease_until, now).await {
                    Ok(Some(claimed)) => {
                        self.logger.event(
                            Level::Debug,
                            "task.claimed",
                            "task claimed",
                            &[("task", claimed.id.as_str()), ("kind", &claimed.kind)],
                        );
                        let execution = Execution {
                            store: self.store.clone(),
                            executor: self.executors.get(&claimed.kind).cloned(),
                            owner: self.owner.clone(),
                            lease: self.lease,
                            logger: self.logger.clone(),
                        };
                        let delivery = Delivery {
                            id: claimed.id,
                            lane: claimed.lane,
                            attempt: claimed.attempt,
                            payload: claimed.payload,
                        };
                        running.spawn(execution.run(delivery));
                    }
                    Ok(None) => break,
                    Err(error) => failure = Some(error),
                }
            }
            let Some(ended) = running.join_next().await else {
                break;
            };
            if ended.is_err() {
                self.logger.event(
                    Level::Warn,
                    "task.lost",
                    "executor panicked; the lease will lapse",
                    &[],
                );
            }
            done += 1;
        }
        failure.map_or(Ok(done), Err)
    }

    /// Repeats [`Self::pass`] every `cadence`, logging store failures; runs
    /// until the caller drops or aborts it.
    pub async fn run(&self, cadence: Duration) {
        loop {
            if let Err(error) = self.pass().await {
                self.logger.event(
                    Level::Warn,
                    "task.store",
                    "queue store failed",
                    &[("error", &error.to_string())],
                );
            }
            tokio::time::sleep(cadence).await;
        }
    }
}

/// One claimed task's execution and settlement.
struct Execution {
    store: Arc<dyn Store>,
    executor: Option<Arc<dyn Executor>>,
    owner: String,
    lease: Duration,
    logger: Logger,
}

impl Execution {
    async fn run(self, delivery: Delivery) {
        let id = delivery.id.clone();
        let ack = match &self.executor {
            None => Ack::Reject("no executor for this kind".into()),
            Some(executor) => {
                match tokio::time::timeout(self.lease, executor.execute(delivery)).await {
                    Ok(ack) => ack,
                    Err(_) => {
                        self.logger.event(
                            Level::Warn,
                            "task.lost",
                            "execution outlived its lease",
                            &[("task", id.as_str())],
                        );
                        return;
                    }
                }
            }
        };
        let settlement = match ack {
            Ack::Done(output) => Settlement::Finish {
                outcome: Outcome::Succeeded,
                output,
                error: None,
            },
            Ack::Reject(error) => Settlement::Finish {
                outcome: Outcome::Rejected,
                output: None,
                error: Some(error),
            },
            Ack::Ambiguous(error) => Settlement::Finish {
                outcome: Outcome::Ambiguous,
                output: None,
                error: Some(error),
            },
            Ack::Retry(error) => Settlement::Retry { error },
        };
        match self.store.settle(&id, &self.owner, settlement, now()).await {
            Ok(()) => self.logger.event(
                Level::Debug,
                "task.settled",
                "task settled",
                &[("task", id.as_str())],
            ),
            Err(error) => self.logger.event(
                Level::Warn,
                "task.lost",
                "settlement not stored",
                &[("task", id.as_str()), ("error", &error.to_string())],
            ),
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
