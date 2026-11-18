//! [`RateProvider`]: Bitcoin's fee rate now, from the node's mempool
//! minimum and conservative estimate.

use base::{FeeRateProvider, TransactionError};

use crate::{FeeRate, rpc::Client};

/// Bitcoin's `base::FeeRateProvider`: the base is the mempool's minimum,
/// the tip the node's conservative rate for confirmation within
/// `target_blocks` above it. Without an estimate (a fresh node), the floor
/// is the total. Two calls.
pub(crate) struct RateProvider {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) target_blocks: u16,
    pub(crate) floor: FeeRate,
}

impl base::Networked for RateProvider {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl FeeRateProvider for RateProvider {
    fn rate(&self) -> base::Future<'_, Result<base::FeeRate, TransactionError>> {
        Box::pin(async move {
            let minimum = self.rpc.mempool_minimum().await;
            let base = minimum
                .map_err(TransactionError::unavailable)?
                .sat_per_kvb();
            let estimate = self.rpc.fee_rate(self.target_blocks).await;
            let total = estimate
                .map_err(TransactionError::unavailable)?
                .unwrap_or(self.floor)
                .sat_per_kvb();
            Ok(base::FeeRate {
                base: base.into(),
                tip: total.saturating_sub(base).into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::FeeRateProvider as _;
    use rpc::testing::{Reply, Script};
    use serde_json::json;

    use super::RateProvider;
    use crate::{Client, FeeRate, Network};

    fn provider(script: &Script) -> RateProvider {
        RateProvider {
            network: Network::Regtest.into(),
            rpc: Client::new(Arc::new(script.clone()), Network::Regtest),
            target_blocks: 1,
            floor: FeeRate::from_sat_per_kvb(1_500),
        }
    }

    #[tokio::test]
    async fn the_base_is_the_mempool_minimum_and_the_tip_the_estimate_above_it() {
        let minimum = |rate| Reply::ok("getmempoolinfo", json!({"mempoolminfee": rate}));
        let script = Script::new([
            minimum(0.00001),
            Reply::ok("estimatesmartfee", json!({"feerate": 0.00003}))
                .expecting(json!([1, "conservative"])),
            minimum(0.00002),
            Reply::ok("estimatesmartfee", json!({"feerate": 0.00001})),
            minimum(0.00001),
            Reply::ok("estimatesmartfee", json!({"errors": ["few data"]})),
        ]);
        let provider = provider(&script);
        let rate = |base, tip| base::FeeRate { base, tip };
        assert_eq!(provider.rate().await.unwrap(), rate(1_000, 2_000));
        assert_eq!(
            provider.rate().await.unwrap(),
            rate(2_000, 0),
            "never below"
        );
        assert_eq!(
            provider.rate().await.unwrap(),
            rate(1_000, 500),
            "the floor"
        );
        script.exhausted();
    }
}
