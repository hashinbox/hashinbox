//! The queue's persistence port. Implementations live with their application
//! (the server's SQLite queue); this crate ships none.

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
pub trait Store: Send + Sync {
    /// Inserts or replaces `job` (its payload wholesale), durable before it
    /// resolves; then publishes it to [`Self::changes`].
    fn set(&self, job: Job) -> StoreFuture<'_, ()>;

    /// The job `id`; `None` when unknown.
    fn get<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Job>>;

    /// Every unfinished (pending or running) job with `run_at <= now`, by id.
    fn next(&self, now: u64) -> StoreFuture<'_, Vec<Job>>;

    /// Every job set from now on. A lagged receiver missed some; it should refetch.
    fn changes(&self) -> broadcast::Receiver<Job>;
}
