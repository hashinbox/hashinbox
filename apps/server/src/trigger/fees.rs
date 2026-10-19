//! [`Fees`]: a network's fee estimates, each made once.

use std::{collections::BTreeMap, sync::Arc};

use base::{Decimal, FeeEstimator};

use crate::models::wallet_index_balance::WalletIndexBalances;

/// A network's fee estimator, awaited on `runtime` (an evaluation runs on a
/// blocking thread), with the estimates made so far.
pub(crate) struct Fees {
    estimator: Arc<dyn FeeEstimator>,
    runtime: tokio::runtime::Handle,
    /// Native atomic fee per set of wallet index ids.
    made: BTreeMap<Vec<String>, Decimal>,
}

impl Fees {
    /// No estimates yet.
    pub(crate) fn new(estimator: Arc<dyn FeeEstimator>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            estimator,
            runtime,
            made: BTreeMap::new(),
        }
    }

    /// The native atomic fee of sending everything `indexes` hold to one
    /// address (empty: 0).
    ///
    /// # Errors
    ///
    /// When the estimator fails.
    pub(crate) fn estimate(&mut self, indexes: &WalletIndexBalances) -> anyhow::Result<Decimal> {
        if indexes.is_empty() {
            return Ok(Decimal::zero());
        }
        let key = indexes.ids();
        if let Some(fee) = self.made.get(&key) {
            return Ok(fee.clone());
        }
        let fee = self
            .runtime
            .block_on(self.estimator.estimate(&indexes.sources()))?;
        self.made.insert(key, fee.clone());
        Ok(fee)
    }
}
