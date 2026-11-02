#![doc = include_str!("../README.md")]

mod jobs;
mod rules;
mod storage;
mod task;
#[cfg(feature = "runner")]
mod worker;

pub use jobs::Jobs;
pub use rules::{Ack, Retry};
pub use storage::{Changes, Store, StoreError, StoreFuture};
pub use task::{Id, Job, Status, Topic, now};
#[cfg(feature = "runner")]
pub use worker::{Task, Ticking, Worker};

use std::{
    fmt,
    future::Future,
    sync::{Mutex, MutexGuard, PoisonError},
};
use tokio::task::{JoinError, JoinSet};

/// Background tasks owned until drained; dropping the owner aborts remaining work.
#[derive(Default)]
pub struct Tasks(Mutex<Owned>);

/// Why a task was refused before spawning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Admission has been closed.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("task admission is closed")
    }
}

impl std::error::Error for Error {}

/// Everything registered and not yet joined.
struct Owned {
    running: JoinSet<()>,
    /// Whether a registration is still accepted.
    open: bool,
    /// The first observed task that panicked, kept for [`Tasks::drain`].
    failure: Option<JoinError>,
}

impl Default for Owned {
    fn default() -> Self {
        Self {
            running: JoinSet::new(),
            open: true,
            failure: None,
        }
    }
}

impl Owned {
    /// Joins every task that has already ended, so a long-lived process holds
    /// the running ones and not every one it ever ran.
    fn reap(&mut self) {
        while let Some(ended) = self.running.try_join_next() {
            if let Err(error) = ended {
                self.failure.get_or_insert(error);
            }
        }
    }
}

impl Tasks {
    /// An open collection with no tasks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns and owns work unless admission has closed.
    ///
    /// # Errors
    /// Returns [`Error::Closed`] without spawning when admission is closed.
    ///
    /// # Panics
    /// Panics outside a Tokio runtime.
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> Result<(), Error> {
        let mut tasks = self.tasks();
        if !tasks.open {
            return Err(Error::Closed);
        }
        tasks.reap();
        tasks.running.spawn(task);
        Ok(())
    }

    /// Refuses every later registration. What is already registered keeps
    /// running until [`Self::drain`] has waited for it.
    ///
    /// A shutdown sequence calls this before it drains its request handlers,
    /// so no handler still in flight hands over work nothing will wait for.
    pub fn close(&self) {
        self.tasks().open = false;
    }

    /// Closes registration and waits for every registered task.
    ///
    /// Waits for every task even if another panics. Call from one shutdown
    /// driver: concurrent drains do not wait for work taken by another drain.
    /// Cancelling this future aborts the tasks it took from the collection.
    ///
    /// # Errors
    ///
    /// The first observed task that panicked, whenever it did.
    pub async fn drain(&self) -> Result<(), JoinError> {
        let (mut running, mut failure) = {
            let mut tasks = self.tasks();
            tasks.open = false;
            (std::mem::take(&mut tasks.running), tasks.failure.take())
        };
        while let Some(ended) = running.join_next().await {
            if let Err(error) = ended {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// The tasks, whatever a panicking holder left behind: nothing runs under
    /// this lock that could leave them half-written.
    fn tasks(&self) -> MutexGuard<'_, Owned> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
