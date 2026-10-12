//! The queue's persistence port and its shipped drivers.

use std::{fmt, future::Future, pin::Pin};

use crate::task::{Claimed, Id, NewTask, Settlement, Status};

#[cfg(feature = "memory")]
pub mod memory;
#[cfg(feature = "sqlite")]
pub mod sqlite;

/// A store operation in flight.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// Why a store operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The caller no longer holds the task's lease, or it is not running.
    Lost,
    /// The input is unsupported (e.g. zero attempts).
    Invalid(String),
    /// The backend failed; retryable as far as the queue knows.
    Backend(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lost => formatter.write_str("the task lease is not held"),
            Self::Invalid(reason) => write!(formatter, "invalid task: {reason}"),
            Self::Backend(reason) => write!(formatter, "queue store failed: {reason}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Durable tasks and their results. Every driver passes `tests/contract.rs`.
///
/// Times are milliseconds since the Unix epoch, supplied by the caller.
pub trait Store: Send + Sync {
    /// Stores `task` as pending; durable before it resolves.
    fn enqueue<'a>(&'a self, task: NewTask, now: u64) -> StoreFuture<'a, Id>;

    /// Finishes heads past their deadline (`Expired`) and lapsed leases with no
    /// attempts left (`Failed`), then leases the oldest due lane head to `owner`
    /// and counts the attempt. A lane whose head is running yields nothing.
    fn claim<'a>(
        &'a self,
        owner: &'a str,
        lease_until: u64,
        now: u64,
    ) -> StoreFuture<'a, Option<Claimed>>;

    /// Settles `owner`'s claim of `id`: terminal, or back to pending at
    /// `now + backoff * attempts` while attempts remain (else `Failed`).
    ///
    /// # Errors
    /// [`StoreError::Lost`] when `owner` does not hold a running lease on `id`.
    fn settle<'a>(
        &'a self,
        id: &'a Id,
        owner: &'a str,
        settlement: Settlement,
        now: u64,
    ) -> StoreFuture<'a, ()>;

    /// Moves a pending task to `Cancelled`; `false` when it is not pending.
    fn cancel<'a>(&'a self, id: &'a Id, now: u64) -> StoreFuture<'a, bool>;

    /// Where `id` is; `None` for an unknown id.
    fn status<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Status>>;
}

/// Refuses a task no attempt could ever run.
#[cfg(any(feature = "memory", feature = "sqlite"))]
fn validate(task: &NewTask) -> Result<(), StoreError> {
    if task.policy.max_attempts == 0 {
        return Err(StoreError::Invalid(
            "max_attempts must be at least 1".into(),
        ));
    }
    Ok(())
}

/// When a retry after `attempts` claims is due.
#[cfg(any(feature = "memory", feature = "sqlite"))]
fn retry_at(now: u64, backoff_ms: u64, attempts: u32) -> u64 {
    now.saturating_add(backoff_ms.saturating_mul(u64::from(attempts)))
}
