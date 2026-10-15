//! [`Cache`]: one value of network state with a time to live, such as the
//! fee prices a chain's estimator keeps.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// One value of network state, refreshed when older than its time to live.
/// Concurrent readers of a stale value await one refresh (single flight);
/// the lock is async, so waiting holds no thread.
#[derive(Debug)]
pub struct Cache<T> {
    ttl: Duration,
    slot: Mutex<Option<(Instant, T)>>,
}

impl<T: Clone + Send> Cache<T> {
    /// An empty cache whose values live `ttl`; zero refreshes every read.
    #[must_use]
    pub const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slot: Mutex::const_new(None),
        }
    }

    /// The cached value while fresh, else the value `refresh` produces, kept
    /// from now on. A failed refresh keeps nothing and fails the read.
    ///
    /// # Errors
    ///
    /// `refresh`'s failure.
    pub async fn get<E, F>(&self, refresh: impl FnOnce() -> F) -> Result<T, E>
    where
        F: std::future::Future<Output = Result<T, E>>,
    {
        let mut slot = self.slot.lock().await;
        if let Some((at, value)) = slot.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(value.clone());
        }
        let value = refresh().await?;
        *slot = Some((Instant::now(), value.clone()));
        Ok(value)
    }

    /// Forgets the value, so the next read refreshes: for a caller that
    /// learns of a new block.
    pub async fn expire(&self) {
        *self.slot.lock().await = None;
    }
}
