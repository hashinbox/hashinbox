//! [`Cached`]: Ethereum's `base::FeeEstimator` that reuses the precise
//! [`Estimator`]'s answers, by Ethereum's rules, so the node is asked once
//! per kind of send per time to live, however many wallets send.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use base::{Cache, Decimal, Holdings, Reason, Spend, Spends, TransactionError};

use crate::fee::{Estimator, Fees, total};

/// Ethereum's cached `base::FeeEstimator`, which every builder of the
/// factory signs with too, so an estimate and the transaction signed after
/// it agree. On Ethereum the price per gas unit is the same for every send,
/// and a transfer's gas depends only on its asset and on whether its
/// recipient holds the asset already (nothing to create), not on who sends
/// it or to which such recipient. So it keeps, each for `ttl`:
///
/// - the price per gas unit, one per network;
/// - the gas of each (asset, recipient holds it), filled by the first spend
///   of that kind to miss.
pub(crate) struct Cached {
    pub(crate) inner: Estimator,
    /// Who holds what: which kind each spend is.
    pub(crate) holdings: Arc<dyn Holdings>,
    fees: Cache<Fees>,
    gas: BTreeMap<(Option<alloy_primitives::Address>, bool), Cache<u64>>,
}

impl Cached {
    /// Over `inner`, its answers kept `ttl`, kinds of send as `holdings` say.
    pub(crate) fn new(inner: Estimator, holdings: Arc<dyn Holdings>, ttl: Duration) -> Self {
        let gas = std::iter::once(None)
            .chain(inner.tokens.iter().map(Some))
            .flat_map(|contract| [(contract, true), (contract, false)])
            .map(|kind| (kind, Cache::new(ttl)))
            .collect();
        Self {
            inner,
            holdings,
            fees: Cache::new(ttl),
            gas,
        }
    }

    /// The price per gas unit: kept, else asked.
    ///
    /// # Errors
    ///
    /// The precise estimator's failure.
    pub(crate) async fn fees(&self) -> Result<Fees, TransactionError> {
        self.fees.get(|| self.inner.fees()).await
    }

    /// Per spend, in order, whether its recipient holds its asset: which
    /// kind of send it is. One read.
    ///
    /// # Errors
    ///
    /// The holdings' failure.
    pub(crate) async fn holds(&self, spends: &Spends) -> Result<Vec<bool>, TransactionError> {
        self.holdings
            .holds(spends)
            .await
            .map_err(TransactionError::unavailable)
    }

    /// The gas limit of `spend`, whose recipient holds its asset when
    /// `held`: kept for its kind, else asked for it. A sender holding none of
    /// the token cannot be asked for (the node runs the transfer), so it
    /// gets the kind's last answer however old: the sender does not change
    /// the gas.
    ///
    /// # Errors
    ///
    /// An asset that is neither ether nor an allowlisted token, or the
    /// precise estimator's failure with no earlier answer to fall back on.
    pub(crate) async fn gas(&self, spend: &Spend, held: bool) -> Result<u64, TransactionError> {
        let contract = self.inner.contract(&spend.asset)?;
        let held = held && spend.to.is_some();
        let Some(cache) = self.gas.get(&(contract, held)) else {
            return Err(TransactionError::failed("no gas kept for this asset"));
        };
        match cache.get(|| self.inner.gas(spend)).await {
            Err(error) if error.reason == Some(Reason::Reverted) => cache.last().await.ok_or(error),
            answer => answer,
        }
    }
}

impl base::Networked for Cached {
    fn network(&self) -> &base::Network {
        &self.inner.network
    }
}

impl base::FeeEstimator for Cached {
    /// Σ per spend `gas limit × max fee per gas`, each sender its own
    /// transaction: exactly the fee leg a builder signs.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let held = self.holds(spends).await?;
            let fees = self.fees().await?;
            let mut gas = Vec::with_capacity(spends.len());
            for (spend, held) in spends.iter().zip(held) {
                gas.push(self.gas(spend, held).await?);
            }
            total(&self.inner.limits, fees, gas)
        })
    }
}
