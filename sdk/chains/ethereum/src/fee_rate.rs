//! [`RateProvider`]: Ethereum's fee rate now, the latest base fee and the
//! node's tip.

use base::{FeeRateProvider, TransactionError};

use crate::{Client, fee::fee_cap};

/// Ethereum's `base::FeeRateProvider`: the latest base fee and the node's
/// tip. Two calls.
pub(crate) struct RateProvider {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
}

impl base::Networked for RateProvider {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl FeeRateProvider for RateProvider {
    /// A quote above 128 bits: `FeeCap`.
    fn rate(&self) -> base::Future<'_, Result<base::FeeRate, TransactionError>> {
        Box::pin(async move {
            let (base, tip) = self
                .rpc
                .fees()
                .await
                .map_err(TransactionError::unavailable)?;
            let wide = || fee_cap(anyhow::anyhow!("Ethereum fee per gas exceeds 128 bits"));
            Ok(base::FeeRate {
                base: u128::try_from(base).map_err(|_| wide())?,
                tip: u128::try_from(tip).map_err(|_| wide())?,
            })
        })
    }
}
