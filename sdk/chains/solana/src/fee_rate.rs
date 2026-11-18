//! [`RateProvider`]: Solana's fee rate now, the median of the network's
//! recent priority fees.

use base::{FeeRateProvider, TransactionError};

use crate::{Client, fee::FLOOR};

/// Solana's `base::FeeRateProvider`: no base; the tip the median of the
/// network's recent priority fees, one per slot the node keeps, never below
/// [`FLOOR`]. One call.
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
    fn rate(&self) -> base::Future<'_, Result<base::FeeRate, TransactionError>> {
        Box::pin(async move {
            let mut fees = self
                .rpc
                .priority_fees()
                .await
                .map_err(TransactionError::unavailable)?;
            fees.sort_unstable();
            let median = fees.get(fees.len().saturating_sub(1) / 2).copied();
            Ok(base::FeeRate {
                base: 0,
                tip: median.unwrap_or(0).max(FLOOR).into(),
            })
        })
    }
}
