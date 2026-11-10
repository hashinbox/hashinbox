//! The queue's persistence port ([`Store`]), its change signal
//! ([`Changes`]) and [`Notifying`], a store that signals its writes. [`MemoryStore`](crate::MemoryStore) is the default store; a
//! durable one lives with its application and passes
//! [`store_contract`](crate::testing::store_contract).

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::sync::watch;

use crate::{
    job::{Id, Job, Status},
    worker::OnLock,
};

/// A store operation in flight.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// Why a store operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The job cannot be stored as it is (e.g. a time above the storable
    /// range); writing it again fails again.
    Invalid(String),
    /// The backend failed; retryable as far as the queue knows.
    Backend(String),
    /// The run a state belongs to has ended: the job no longer runs at that
    /// attempt, so the state is not written.
    Stale,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(formatter, "invalid job: {reason}"),
            Self::Backend(reason) => write!(formatter, "queue store failed: {reason}"),
            Self::Stale => formatter.write_str("the run has ended"),
        }
    }
}

impl std::error::Error for StoreError {}

/// What [`Store::enqueue`] did with a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Stored as this job.
    Queued(Id),
    /// Not stored: this unfinished job of the same kind has its topic.
    Duplicate(Id),
}

/// Where jobs are kept, and the queue's rules over many jobs: what is due,
/// what may start, what has expired. A durable store runs them in its own
/// query language; [`MemoryStore`](crate::MemoryStore) is the reference.
/// Times are milliseconds since the Unix epoch.
///
/// Line order is [`Id`] order. Two jobs conflict when their topics do
/// ([`crate::Topic::conflicts`]). A job *holds its place* while it runs, or
/// while it is pending and either due or retried before (`attempts > 0`):
/// a job waiting for a fresh run blocks nobody until it is due.
///
/// A claimed job runs under a lease: its `run_at` is when the lease ends.
/// Past it the job may be claimed again, as if pending, so a run whose
/// outcome was never stored (a dead process, a write that failed) heals by
/// itself.
///
/// A store retries its own transient failures; an error it returns is
/// final for that call.
pub trait Store: Send + Sync {
    /// Stores `job` as the next [`Id`]; when `unique`, refuses it while an
    /// unfinished job of its kind has its topic. Atomic.
    fn enqueue(&self, job: Job, unique: bool) -> StoreFuture<'_, Admission>;

    /// Marks the jobs of `claim.kind` past their deadline failed `expired`
    /// ([`Job::is_expired`]); then marks up to `claim.limit` of its jobs
    /// running until `claim.until`, oldest first, and returns them, their
    /// attempt counted. A job is claimed when it is due ([`Job::is_due`]) and no
    /// conflicting job ahead of it holds its place. With [`OnLock::Skip`], a
    /// job a newer conflicting unfinished job follows is marked skipped
    /// instead (`skipped: {id}`). Atomic: no job is claimed twice.
    fn claim(&self, claim: Claim<'_>) -> StoreFuture<'_, Vec<Job>>;

    /// Stores `job` (a run's saved state, or how it ended) only while the
    /// stored job runs the claim of `attempt` (its `attempts` when
    /// claimed); [`StoreError::Stale`] otherwise, so a write of a run that
    /// already ended never undoes how it ended.
    fn update(&self, job: Job, attempt: u32) -> StoreFuture<'_, ()>;

    /// The job `id`; `None` when unknown.
    fn get(&self, id: Id) -> StoreFuture<'_, Option<Job>>;
}

/// What [`Store::claim`] claims.
#[derive(Clone, Copy, Debug)]
pub struct Claim<'a> {
    /// The kind of job.
    pub kind: &'a str,
    /// At most this many.
    pub limit: usize,
    /// Whether a job gives way to a newer conflicting one.
    pub on_lock: OnLock,
    /// The time due is judged at.
    pub now: u64,
    /// When the claimed runs' lease ends: their `run_at`.
    pub until: u64,
}

/// When a kind's queue should look at the store again.
pub trait Changes: Send + Sync {
    /// Marked changed when what `kind` may start could have changed: a job
    /// of `kind` enqueued, or any run ended (it may unblock a job of
    /// `kind`). It carries nothing: a look at the store tells what changed,
    /// so changes seen together are seen as one.
    fn changes(&self, kind: &str) -> watch::Receiver<()>;
}

/// `inner`, signalling each kind after every write that may change what it
/// starts. Clones share the signals.
#[derive(Clone)]
pub struct Notifying<S> {
    inner: S,
    kinds: Arc<Mutex<HashMap<String, watch::Sender<()>>>>,
}

impl<S> Notifying<S> {
    /// `inner`, signalling its writes.
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            kinds: Arc::default(),
        }
    }

    /// Signals every kind: for jobs written outside [`Store`].
    pub fn notify(&self) {
        for sender in self.kinds().values() {
            sender.send_replace(());
        }
    }

    /// Signals `kind`.
    fn notify_kind(&self, kind: &str) {
        if let Some(sender) = self.kinds().get(kind) {
            sender.send_replace(());
        }
    }

    /// Each kind's signal, whatever a panicking holder left: every write is
    /// one insert.
    fn kinds(&self) -> MutexGuard<'_, HashMap<String, watch::Sender<()>>> {
        self.kinds.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<S: Store> Store for Notifying<S> {
    fn enqueue(&self, job: Job, unique: bool) -> StoreFuture<'_, Admission> {
        Box::pin(async move {
            let kind = job.kind.clone();
            let admission = self.inner.enqueue(job, unique).await?;
            if matches!(admission, Admission::Queued(_)) {
                self.notify_kind(&kind);
            }
            Ok(admission)
        })
    }

    fn claim(&self, claim: Claim<'_>) -> StoreFuture<'_, Vec<Job>> {
        self.inner.claim(claim)
    }

    fn update(&self, job: Job, attempt: u32) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let ended = job.status != Status::Running;
            self.inner.update(job, attempt).await?;
            // A run that ended frees its topic, for any kind, and its slot.
            if ended {
                self.notify();
            }
            Ok(())
        })
    }

    fn get(&self, id: Id) -> StoreFuture<'_, Option<Job>> {
        self.inner.get(id)
    }
}

impl<S: Send + Sync> Changes for Notifying<S> {
    fn changes(&self, kind: &str) -> watch::Receiver<()> {
        self.kinds()
            .entry(kind.to_owned())
            .or_insert_with(|| watch::Sender::new(()))
            .subscribe()
    }
}
