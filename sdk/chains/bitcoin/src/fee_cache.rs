//! [`Cached`]: Bitcoin's `base::FeeEstimator` that reuses the precise
//! [`Estimator`]'s fee rate, so the node is asked once per time to live.

use std::time::Duration;

use base::{Cache, Decimal, Spends, TransactionError};

use crate::fee::{Estimator, FeeRate};

/// Bitcoin's cached `base::FeeEstimator`, which every builder of the factory
/// prices with too, so an estimate and the transaction signed after it
/// agree. A send costs the fee rate, the same for every send, times its size,
/// which depends on each sender's coins (read from the store, not the node).
/// So it keeps only the rate, for `ttl`.
pub(crate) struct Cached {
    pub(crate) inner: Estimator,
    rate: Cache<FeeRate>,
}

impl Cached {
    /// Over `inner`, its rate kept `ttl`.
    pub(crate) const fn new(inner: Estimator, ttl: Duration) -> Self {
        Self {
            inner,
            rate: Cache::new(ttl),
        }
    }

    /// The fee rate: kept, else asked.
    ///
    /// # Errors
    ///
    /// The precise estimator's failure.
    pub(crate) async fn rate(&self) -> Result<FeeRate, TransactionError> {
        self.rate.get(|| self.inner.rate()).await
    }
}

impl base::Networked for Cached {
    fn network(&self) -> &base::Network {
        &self.inner.network
    }
}

impl base::FeeEstimator for Cached {
    /// The fee of `spends` at the kept rate.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move { self.inner.fee(spends, self.rate().await?).await })
    }
}
