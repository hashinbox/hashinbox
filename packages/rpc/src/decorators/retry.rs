use std::{num::NonZeroU32, time::Duration};

use crate::{Answer, Batch, BoxFuture, Client, Endpoint, Error, RawJson, Repetition, Request};

/// Delay between two [`Retry`] attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backoff {
    /// The same delay after every failed attempt.
    Fixed(Duration),
    /// Doubles from `initial`, never past `max`, which also caps `initial`.
    Exponential {
        /// Delay after the first failed attempt.
        initial: Duration,
        /// Upper bound of every delay.
        max: Duration,
    },
}

impl Backoff {
    /// The delay after attempt number `attempt`, counted from one.
    fn delay(self, attempt: u32) -> Duration {
        match self {
            Self::Fixed(delay) => delay,
            Self::Exponential { initial, max } => {
                let multiplier = 1_u32 << attempt.saturating_sub(1).min(31);
                initial.checked_mul(multiplier).unwrap_or(max).min(max)
            }
        }
    }
}

impl Default for Backoff {
    /// 250 ms doubling to a 2 s cap: the policy the transport has always used.
    fn default() -> Self {
        Self::Exponential {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(2),
        }
    }
}

/// Repeats the work of an inner [`Client`] while it fails with an
/// [`Error::is_retryable`] error.
///
/// One attempt is one complete execution of the inner client, so over a
/// client that walks several nodes it retries whole walks and waits between
/// walks, not between nodes.
///
/// [`Repetition::Forbidden`] work reaches the inner client exactly once,
/// whatever the policy says. A batch repeats whole, so only on a failure of
/// the whole batch, never on one call's error object. A broadcast stays a
/// broadcast: each attempt is one [`Client::broadcast`] of the inner client.
/// Each repeat is logged as `rpc.retry` (`method attempt reason`), debug.
#[derive(Clone)]
pub struct Retry<C> {
    inner: C,
    attempts: NonZeroU32,
    backoff: Backoff,
}

impl<C: Client> Retry<C> {
    /// Three attempts, 250 ms doubling to a 2 s cap.
    #[must_use]
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            attempts: NonZeroU32::new(3).expect("three is nonzero"),
            backoff: Backoff::default(),
        }
    }

    /// Sets how many attempts one call may make.
    #[must_use]
    pub fn with_attempts(mut self, attempts: NonZeroU32) -> Self {
        self.attempts = attempts;
        self
    }

    /// Sets the delay between attempts.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// `send` until it answers, fails for good or runs out of attempts;
    /// once for [`Repetition::Forbidden`] work.
    async fn repeat<'a, T>(
        &'a self,
        method: &str,
        repetition: Repetition,
        send: impl Fn() -> BoxFuture<'a, Result<T, Error>>,
    ) -> Result<T, Error> {
        if repetition == Repetition::Forbidden {
            return send().await;
        }
        // `attempts` is nonzero, so the loop always produces an outcome and the
        // last one is the answer: there is no "no attempt was made" case.
        let mut attempt = 1;
        loop {
            let outcome = send().await;
            let repeats = matches!(&outcome, Err(error) if error.is_retryable());
            if !repeats || attempt == self.attempts.get() {
                return outcome;
            }
            if let Err(error) = &outcome {
                let reason = error.label();
                tracing::debug!(target: crate::events::RPC_RETRY, method, attempt, reason);
            }
            tokio::time::sleep(self.backoff.delay(attempt)).await;
            attempt += 1;
        }
    }
}

impl<C: Client> Client for Retry<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        let (method, repetition) = (request.method().to_owned(), request.repetition());
        Box::pin(async move {
            let send = || self.inner.request(request.clone());
            self.repeat(&method, repetition, send).await
        })
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        let method = batch.method().unwrap_or("batch").to_owned();
        let repetition = batch.repetition();
        Box::pin(async move {
            let send = || self.inner.batch(batch.clone());
            self.repeat(&method, repetition, send).await
        })
    }

    /// Repeats whole broadcasts under the same policy; [`Repetition::Forbidden`]
    /// work is broadcast once.
    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        let (method, repetition) = (request.method().to_owned(), request.repetition());
        Box::pin(async move {
            let send = || self.inner.broadcast(request.clone());
            self.repeat(&method, repetition, send).await
        })
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}
