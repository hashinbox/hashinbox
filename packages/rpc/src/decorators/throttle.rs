//! [`Throttle`]: at most `rpc.rps` calls a second to one node.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::time::Instant;

use crate::{Answer, Batch, BoxFuture, Client, Endpoint, Error, RawJson, Request, Url};

/// What [`Throttle`] reads from a node's URL.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ThrottleParams {
    /// `rpc.rps`: the most calls a second this node is sent, a positive
    /// number (`3`, `3.5`). `None` when unwritten: no limit.
    pub rps: Option<f64>,
}

impl TryFrom<&Url> for ThrottleParams {
    type Error = Error;

    fn try_from(url: &Url) -> Result<Self, Error> {
        let Some(value) = url.params.get("rpc.rps") else {
            return Ok(Self::default());
        };
        let rps = value
            .parse()
            .ok()
            .filter(|rps| spacing(*rps).is_some())
            .ok_or_else(|| {
                crate::endpoint::invalid("JSON-RPC endpoint `rpc.rps` must be a positive number")
            })?;
        Ok(Self { rps: Some(rps) })
    }
}

/// How long one call's token takes to refill at `rps` a second; `None`
/// unless `rps` is a positive finite number.
fn spacing(rps: f64) -> Option<Duration> {
    (rps.is_finite() && rps > 0.0)
        .then(|| Duration::try_from_secs_f64(rps.recip()).ok())
        .flatten()
}

/// `C`, sent at most `rps` calls a second: tokens refill at `rps`, and a
/// request takes one per call it carries (a batch, its length), waiting until
/// the tokens taken before it have refilled. An idle node's first request
/// goes at once. It waits, never fails, for the limit. Clones share the
/// bucket. `None`: no limit. A wait is logged as `rpc.wait` (`calls
/// wait_ms`), debug, and is what [`crate::Traced`] under it reports as the
/// request's `wait_ms` ([`waited`]).
///
/// [`crate::node`] puts it under [`crate::Split`], so it paces each request
/// sent, and over [`crate::Http`], so a wait never counts toward a request's
/// timeout.
#[derive(Clone)]
pub struct Throttle<C> {
    inner: C,
    bucket: Option<Arc<Bucket>>,
}

/// The tokens taken so far, as the time they have all refilled.
#[derive(Debug)]
struct Bucket {
    /// One call's refill time: `1 / rps`.
    spacing: Duration,
    /// What `refilled` counts from.
    origin: Instant,
    /// When every token taken so far has refilled, since `origin`.
    refilled: Mutex<Duration>,
}

impl Bucket {
    /// Waits until the tokens taken before have refilled, having taken
    /// `calls` more; how long.
    async fn take(&self, calls: usize) -> Duration {
        let now = self.origin.elapsed();
        let start = {
            let mut refilled = self.refilled.lock().unwrap_or_else(PoisonError::into_inner);
            let start = (*refilled).max(now);
            let calls = u32::try_from(calls).unwrap_or(u32::MAX);
            *refilled = start.saturating_add(self.spacing.saturating_mul(calls));
            start
        };
        tokio::time::sleep(start - now).await;
        start - now
    }
}

tokio::task_local! {
    /// How long the request being sent waited for its rate.
    static WAITED: Duration;
}

/// How long the request now sent waited in a [`Throttle`] above; zero
/// under none.
pub(crate) fn waited() -> Duration {
    WAITED.try_with(|waited| *waited).unwrap_or_default()
}

impl<C> Throttle<C> {
    /// `inner`, sent at most `rps` calls a second.
    #[must_use]
    pub fn new(inner: C, rps: Option<f64>) -> Self {
        let bucket = rps.and_then(spacing).map(|spacing| {
            Arc::new(Bucket {
                spacing,
                origin: Instant::now(),
                refilled: Mutex::new(Duration::ZERO),
            })
        });
        Self { inner, bucket }
    }

    /// Waits for `calls` tokens; how long.
    async fn wait(&self, calls: usize) -> Duration {
        let Some(bucket) = &self.bucket else {
            return Duration::ZERO;
        };
        let waited = bucket.take(calls).await;
        if !waited.is_zero() {
            let wait_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
            tracing::debug!(target: "rpc.wait", calls, wait_ms, "RPC request waited for its rate");
        }
        waited
    }
}

impl<C: Client> Client for Throttle<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(async move {
            let waited = self.wait(1).await;
            WAITED.scope(waited, self.inner.request(request)).await
        })
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(async move {
            let waited = self.wait(batch.len()).await;
            WAITED.scope(waited, self.inner.batch(batch)).await
        })
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let waited = self.wait(1).await;
            WAITED.scope(waited, self.inner.broadcast(request)).await
        })
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}
