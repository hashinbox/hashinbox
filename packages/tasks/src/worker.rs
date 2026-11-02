//! What runs a job: a [`Worker`] per kind and the [`Task`] it is handed; and
//! a timer for [`Changes`] ([`Ticking`]). The executor is the caller's.

use std::{future::Future, sync::Arc, time::Duration};

use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::broadcast;

use crate::{
    rules::Ack,
    storage::{Changes, Store, StoreError},
    task::Job,
};

/// Does one kind of job.
pub trait Worker: Send + Sync + 'static {
    /// The kind name stored on its jobs.
    const KIND: &'static str;
    /// What to do.
    type Input: DeserializeOwned + Send;
    /// What is done so far; `()` when a rerun needs nothing.
    type State: Serialize + DeserializeOwned + Send;

    /// Runs one job and answers it.
    fn run(&self, task: Task<Self::Input, Self::State>) -> impl Future<Output = Ack> + Send;
}

/// One running job, as its worker sees it.
pub struct Task<I, S> {
    job: Job,
    input: I,
    state: Option<S>,
    store: Arc<dyn Store>,
}

impl<I: DeserializeOwned, S: DeserializeOwned> Task<I, S> {
    /// The task of running `job`, its input and state decoded; a custom
    /// executor makes one to call [`Worker::run`].
    ///
    /// # Errors
    /// `Ack::Fail` with `invalid-input` or `invalid-state` when either does
    /// not decode.
    pub fn new(job: Job, store: Arc<dyn Store>) -> Result<Self, Ack> {
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

impl<I, S: Serialize> Task<I, S> {
    /// The job: id, topic, attempts, reason, deadline. Read only.
    pub fn job(&self) -> &Job {
        &self.job
    }

    /// What to do.
    pub fn input(&self) -> &I {
        &self.input
    }

    /// The last saved state; `None` on a first run.
    pub fn state(&self) -> Option<&S> {
        self.state.as_ref()
    }

    /// Stores `state` now; a rerun starts from it.
    ///
    /// # Errors
    /// When `state` does not serialize or the store fails.
    pub async fn save(&mut self, state: S) -> Result<(), StoreError> {
        let value =
            serde_json::to_value(&state).map_err(|error| StoreError::Invalid(error.to_string()))?;
        self.job.state = Some(value);
        self.store.update(self.job.clone()).await?;
        self.state = Some(state);
        Ok(())
    }

    /// Done.
    #[must_use]
    pub fn ack(self) -> Ack {
        Ack::Done
    }

    /// Retried while attempts are left.
    #[must_use]
    pub fn nack(self, reason: impl Into<String>) -> Ack {
        Ack::Retry(reason.into())
    }

    /// Failed, not retried.
    #[must_use]
    pub fn fail(self, reason: impl Into<String>) -> Ack {
        Ack::Fail(reason.into())
    }

    /// Done for now; runs again at `at` (Unix ms) as fresh.
    #[must_use]
    pub fn again(self, at: u64) -> Ack {
        Ack::Again(at)
    }
}

/// Another [`Changes`]' signal, plus one every `period`: a retry falls due
/// with no write.
pub struct Ticking {
    sender: broadcast::Sender<()>,
    timer: tokio::task::JoinHandle<()>,
}

impl Ticking {
    /// `inner`'s changes and a tick every `period`.
    ///
    /// # Panics
    /// Outside a Tokio runtime.
    #[must_use]
    pub fn new(inner: &dyn Changes, period: Duration) -> Self {
        let sender = broadcast::channel(16).0;
        let mut from = inner.changes();
        let to = sender.clone();
        let timer = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(period);
            loop {
                tokio::select! {
                    received = from.recv() => {
                        if matches!(received, Err(broadcast::error::RecvError::Closed)) {
                            from = from.resubscribe();
                            ticks.tick().await;
                        }
                    }
                    _ = ticks.tick() => {}
                }
                // No subscriber is not an error.
                let _ = to.send(());
            }
        });
        Self { sender, timer }
    }
}

impl Changes for Ticking {
    fn changes(&self) -> broadcast::Receiver<()> {
        self.sender.subscribe()
    }
}

impl Drop for Ticking {
    fn drop(&mut self) {
        self.timer.abort();
    }
}
