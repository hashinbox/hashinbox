//! What runs a job: a [`Worker`] per kind, its [`Policy`], the [`Context`]
//! it is handed and the [`Ack`] it answers.
//! The `Executor` runs them.

use std::{
    future::Future,
    num::{NonZeroU32, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    job::Job,
    storage::{Admission, Store, StoreError, StoreFuture},
    topic::Topic,
};

/// How a worker answered a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ack {
    /// Succeeded.
    Done,
    /// Succeeded, and had nothing to do.
    Idle,
    /// Failed for good, with why.
    Fail(String),
    /// Run again after the backoff while attempts remain, with why not now.
    Retry(String),
    /// Run again at this Unix ms as fresh (attempts cleared), keeping why
    /// when given. A recurring job is one that answers this instead of
    /// `Done`; it never runs out.
    Again(u64, Option<String>),
}

impl Ack {
    /// Its name: `done`, `idle`, `fail`, `retry` or `again`.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Fail(_) => "fail",
            Self::Retry(_) => "retry",
            Self::Again(..) => "again",
        }
    }

    /// Whether the run failed: for good, or for a retry.
    #[must_use]
    pub const fn failed(&self) -> bool {
        matches!(self, Self::Fail(_) | Self::Retry(_))
    }
}

/// How retries go: at most `attempts` executions, a retry after attempt `n`
/// waiting `backoff × n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Executions, the first included.
    pub attempts: NonZeroU32,
    /// The wait per attempt made.
    pub backoff: Duration,
}

impl Retry {
    /// Three attempts, 250 ms backoff.
    pub const DEFAULT: Self = Self {
        attempts: NonZeroU32::new(3).unwrap(),
        backoff: Duration::from_millis(250),
    };
}

impl Default for Retry {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How a worker's jobs run: declared by the worker, applied by the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// What a job does while a conflicting one holds its topic.
    pub on_lock: OnLock,
    /// At most one unfinished job of the kind per topic: enqueueing another
    /// answers the one there ([`crate::Admission::Duplicate`]).
    pub unique: bool,
    /// Runs of this kind at once.
    pub concurrency: NonZeroUsize,
    /// Longest a run may take; past it the run is dropped and retried, and
    /// its claim's lease ends soon after.
    pub timeout: Duration,
    /// How failed runs retry.
    pub retry: Retry,
}

impl Policy {
    /// Waiting in line, not unique, one at a time, a one-minute timeout,
    /// the default retry.
    pub const DEFAULT: Self = Self {
        on_lock: OnLock::Wait,
        unique: false,
        concurrency: NonZeroUsize::MIN,
        timeout: Duration::from_secs(60),
        retry: Retry::DEFAULT,
    };
}

impl Default for Policy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a job does while a conflicting job holds its topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnLock {
    /// Waits in line behind earlier conflicting jobs.
    Wait,
    /// Is finished `skipped`, naming the holder, when a conflicting job
    /// holds its topic as it falls due: it does not wait.
    Skip,
}

/// Does one kind of job.
pub trait Worker: Send + Sync + 'static {
    /// The kind name stored on its jobs.
    const KIND: &'static str;
    /// How its jobs run.
    const POLICY: Policy;
    /// What to do.
    type Input: Serialize + DeserializeOwned + Send;
    /// What is done so far; `()` when a rerun needs nothing.
    type State: Serialize + DeserializeOwned + Send;

    /// What a job of `input` locks; `None` when `input` makes no topic.
    fn topic(input: &Self::Input) -> Option<Topic>;

    /// Runs one job and answers it.
    fn run(&self, cx: Context<'_, Self::Input, Self::State>) -> impl Future<Output = Ack> + Send;

    /// Runs once when the worker's queue starts, before it claims: where a
    /// worker enqueues the jobs that must always exist (its recurring
    /// ones, `unique`, so a restart adds none). Nothing by default; a
    /// failure is tried again until it succeeds.
    ///
    /// # Errors
    /// The store's, or the worker's own as [`StoreError::Backend`].
    fn before(&self, store: &dyn Store) -> impl Future<Output = Result<(), StoreError>> + Send {
        let _ = store;
        std::future::ready(Ok(()))
    }

    /// A job of `input`, due now.
    ///
    /// # Errors
    /// [`StoreError::Invalid`] when `input` makes no topic or does not
    /// serialize.
    fn job(input: &Self::Input) -> Result<Job, StoreError> {
        let topic = Self::topic(input)
            .ok_or_else(|| StoreError::Invalid(format!("a {} input makes no topic", Self::KIND)))?;
        let input =
            serde_json::to_value(input).map_err(|error| StoreError::Invalid(error.to_string()))?;
        Ok(Job::new(topic, Self::KIND, input))
    }

    /// Enqueues a job of `input` in `store`, under the kind's
    /// [`Policy::unique`].
    fn enqueue<'a>(store: &'a dyn Store, input: &Self::Input) -> StoreFuture<'a, Admission> {
        match Self::job(input) {
            Ok(job) => store.enqueue(job, Self::POLICY.unique),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }
}

/// One running job, as its worker sees it: the job it borrows from its
/// [`Task`](crate::Task), so what it saves is what the run's outcome stores.
pub struct Context<'a, I, S> {
    job: &'a mut Job,
    input: I,
    state: Option<S>,
    store: Arc<dyn Store>,
}

impl<'a, I: DeserializeOwned, S: DeserializeOwned> Context<'a, I, S> {
    /// The context of running `job`, its input and state decoded.
    ///
    /// # Errors
    /// `Ack::Fail` with `invalid-input` or `invalid-state` when either does
    /// not decode.
    pub fn new(job: &'a mut Job, store: Arc<dyn Store>) -> Result<Self, Ack> {
        let input = serde_json::from_value(job.input.clone())
            .map_err(|_| Ack::Fail("invalid-input".into()))?;
        let state = job
            .state
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| Ack::Fail("invalid-state".into()))?;
        Ok(Self {
            job,
            input,
            state,
            store,
        })
    }
}

impl<I, S: Serialize> Context<'_, I, S> {
    /// The job: id, topic, attempts, reason, deadline.
    pub const fn job(&self) -> &Job {
        self.job
    }

    /// What to do.
    pub const fn input(&self) -> &I {
        &self.input
    }

    /// The last saved state; `None` on a first run.
    pub const fn state(&self) -> Option<&S> {
        self.state.as_ref()
    }

    /// Stores `state` now ([`Store::update`]); a rerun starts from it.
    ///
    /// # Errors
    /// When `state` does not serialize, the store fails, or the run has
    /// ended ([`StoreError::Stale`]).
    pub async fn save(&mut self, state: S) -> Result<(), StoreError> {
        let value =
            serde_json::to_value(&state).map_err(|error| StoreError::Invalid(error.to_string()))?;
        self.job.state = Some(value);
        self.store
            .update(self.job.clone(), self.job.attempts)
            .await?;
        self.state = Some(state);
        Ok(())
    }
}
