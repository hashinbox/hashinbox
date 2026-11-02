use std::{num::NonZeroU32, time::Duration};

use crate::{
    Batch, BoxFuture, CallError, CallResult, Client, Error, Failure, Repetition, Request,
    client::{narrow, widen},
};

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

/// What makes [`Retry`] repeat: which failures count as worth another attempt.
///
/// Distinct from [`Repetition`], which is the *request's* own statement of
/// whether it may be executed twice at all. A transport failure is a trigger
/// under every variant when [`Error::is_retryable`]; the variants differ only
/// in how much of a *received* answer they treat as a transport problem.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Trigger {
    /// Transport failures only: timeouts, unavailable endpoints, and HTTP
    /// 429/502/503/504. The transport's historical behavior.
    #[default]
    Transport,
    /// Also implementation-defined server errors, `-32099..=-32000`
    /// ([`Failure::is_server_error`]).
    Server,
    /// [`Self::Server`], plus these exact remote codes — for a provider that
    /// answers "not ready" or "slow down" with a code of its own.
    Remote(Vec<i64>),
}

impl Trigger {
    /// Whether a received JSON-RPC error object is repeated.
    fn repeats(&self, failure: &Failure) -> bool {
        match self {
            Self::Transport => false,
            Self::Server => failure.is_server_error(),
            Self::Remote(codes) => failure.is_server_error() || codes.contains(&failure.code),
        }
    }
}

/// Repeats the work of an inner [`Client`] under one policy.
///
/// One attempt is one complete execution of the inner client, so
/// `Retry::new(Failover::new(endpoints)?)` retries whole rounds over the
/// endpoint list and waits between rounds, not between endpoints. The reverse
/// nesting, `Failover::new(vec![Retry::new(a), Retry::new(b)])`, exhausts a
/// node before moving on.
///
/// [`Repetition::Forbidden`] work reaches the inner client exactly once,
/// whatever the policy says. A broadcast stays a broadcast: each attempt is one
/// [`Client::broadcast`] of the inner client.
#[derive(Clone, Debug)]
pub struct Retry<C> {
    inner: C,
    attempts: NonZeroU32,
    backoff: Backoff,
    trigger: Trigger,
}

impl<C: Client> Retry<C> {
    /// Three attempts, 250 ms doubling to a 2 s cap, transport failures only.
    #[must_use]
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            attempts: NonZeroU32::new(3).expect("three is nonzero"),
            backoff: Backoff::default(),
            trigger: Trigger::Transport,
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

    /// Sets which failures are repeated.
    #[must_use]
    pub fn with_trigger(mut self, trigger: Trigger) -> Self {
        self.trigger = trigger;
        self
    }

    /// One attempt is one `send` to the inner client: [`Client::request`], or
    /// [`Client::broadcast`] so a broadcast stays one at every attempt.
    async fn repeat_request<'a>(
        &'a self,
        request: Request,
        send: impl Fn(&'a C, Request) -> BoxFuture<'a, Result<CallResult, Error>>,
    ) -> Result<CallResult, Error> {
        if request.repetition() == Repetition::Forbidden {
            return send(&self.inner, request).await;
        }
        // `attempts` is nonzero, so the loop always produces an outcome and the
        // last one is the answer: there is no "no attempt was made" case.
        let mut attempt = 1;
        loop {
            let outcome = send(&self.inner, request.clone()).await;
            if !self.repeats(&outcome) || attempt == self.attempts.get() {
                return outcome;
            }
            tokio::time::sleep(self.backoff.delay(attempt)).await;
            attempt += 1;
        }
    }

    /// A batch repeats on a transport failure only: a per-call error object is
    /// a result, and repeating the batch would re-execute the calls that
    /// succeeded beside it.
    async fn repeat_batch(&self, batch: Batch) -> Result<Vec<CallResult>, Error> {
        if batch.repetition() == Repetition::Forbidden {
            return self.inner.batch(batch).await;
        }
        let mut attempt = 1;
        loop {
            let outcome = self.inner.batch(batch.clone()).await;
            let repeats = matches!(&outcome, Err(error) if error.is_retryable());
            if !repeats || attempt == self.attempts.get() {
                return outcome;
            }
            tokio::time::sleep(self.backoff.delay(attempt)).await;
            attempt += 1;
        }
    }

    fn repeats(&self, outcome: &Result<CallResult, Error>) -> bool {
        match outcome {
            Err(error) => error.is_retryable(),
            Ok(Err(failure)) => self.trigger.repeats(failure),
            Ok(Ok(_)) => false,
        }
    }
}

impl<C: Client> Client for Retry<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        Box::pin(self.repeat_request(request, C::request))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        Box::pin(self.repeat_batch(batch))
    }

    /// Repeats whole broadcasts under the same policy; [`Repetition::Forbidden`]
    /// work is broadcast once.
    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            let send = |client: &'a C, request| -> BoxFuture<'a, _> {
                Box::pin(async move { widen(client.broadcast(request).await) })
            };
            narrow(self.repeat_request(request, send).await)
        })
    }
}
