//! The queue's persistence port and its change signal. Implementations live
//! with their application (the server's SQLite queue); this crate ships none.

use std::{fmt, future::Future, pin::Pin};

use tokio::sync::broadcast;

use crate::task::{Id, Job};

/// A store operation in flight.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// Why a store operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The job is unsupported (e.g. a time above the storable range).
    Invalid(String),
    /// The backend failed; retryable as far as the queue knows.
    Backend(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(formatter, "invalid job: {reason}"),
            Self::Backend(reason) => write!(formatter, "queue store failed: {reason}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Durable jobs, kept whole. Times are milliseconds since the Unix epoch.
///
/// Single process: a job left running by a dead process blocks its topic, so
/// a store sets its running jobs back to pending when it opens.
pub trait Store: Send + Sync {
    /// Adds a new job, durable before it resolves.
    fn enqueue(&self, job: Job) -> StoreFuture<'_, ()>;

    /// Replaces a stored job whole (started, settled, state saved).
    fn update(&self, job: Job) -> StoreFuture<'_, ()>;

    /// The job `id`; `None` when unknown.
    fn get<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Job>>;

    /// What may start now, oldest first: every pending job due at `now`
    /// (`run_at <= now`) with no running job, and no earlier unfinished job,
    /// of a conflicting topic. No two of them conflict.
    fn next(&self, now: u64) -> StoreFuture<'_, Vec<Job>>;
}

/// When to ask [`Store::next`] again.
pub trait Changes: Send + Sync {
    /// Fires when `next` may answer differently: a job enqueued or updated,
    /// or time passed. A lagged receiver missed nothing it needs.
    fn changes(&self) -> broadcast::Receiver<()>;
}
