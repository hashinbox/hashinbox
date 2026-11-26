//! What sending costs on Tron: [`Prices`], the network's resource prices;
//! [`FeeLimits`], the energy a token transfer may burn; [`Estimator`], the
//! worst-case fee of a send and the network's rate, from prices kept a
//! time to live.
//!
//! Tron has no fee market: a transaction burns TRX for the bandwidth
//! (bytes) and energy (contract execution) its sender has no free or staked
//! resources for, and for an account its TRX transfer creates. What a
//! payment burns is only known once mined; before, it is priced at its
//! worst: no free resources, a new account, and a token transfer using all
//! the energy its `fee_limit` buys.

use std::{sync::Arc, time::Duration};

use anyhow::anyhow;
use base::{
    Cache, Decimal, FeeRate, FeeRateProvider, Holdings, Spends, TransactionError,
    TransactionErrorKind,
};
use serde::Deserialize;

use crate::{Client, Tokens};

/// Bandwidth a signed TRX transfer takes, at most: about 270 bytes.
const TRX_BYTES: u64 = 300;

/// Bandwidth a signed TRC-20 transfer takes, at most: about 345 bytes.
const TOKEN_BYTES: u64 = 400;

/// The network's resource prices (`getchainparameters`), in sun.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prices {
    /// Per unit of energy (`getEnergyFee`).
    pub(crate) energy: u64,
    /// Per byte of bandwidth (`getTransactionFee`).
    pub(crate) bandwidth: u64,
    /// Per account a TRX transfer creates (`getCreateAccountFee` and
    /// `getCreateNewAccountFeeInSystemContract`).
    pub(crate) account: u64,
}

/// What a token transfer may burn. Its `fee_limit` is `energy × the energy
/// price`; above `max_fee_limit`, signing fails as `FeeCap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeeLimits {
    /// Energy a token transfer may use. USDT's to a new holder uses about
    /// 130,000 at the highest dynamic-energy factor.
    pub energy: u64,
    /// Largest `fee_limit`, in sun.
    pub max_fee_limit: u64,
}

impl Default for FeeLimits {
    /// 200,000 energy, at most 100 TRX.
    fn default() -> Self {
        Self {
            energy: 200_000,
            max_fee_limit: 100_000_000,
        }
    }
}

/// Tron's `base::FeeEstimator` and `base::FeeRateProvider`, over prices it
/// asks the node for once per `ttl`; builders price with it, so an estimate
/// and the fee leg a builder signs agree.
pub(crate) struct Estimator {
    network: base::Network,
    rpc: Client,
    pub(crate) limits: FeeLimits,
    tokens: Tokens,
    holdings: Arc<dyn Holdings>,
    prices: Cache<Prices>,
}

impl Estimator {
    /// Over `rpc`, sending TRX and `tokens` under `limits`, prices kept
    /// `ttl`, recipients' accounts as `holdings` say.
    pub(crate) fn new(
        rpc: Client,
        limits: FeeLimits,
        tokens: Tokens,
        holdings: Arc<dyn Holdings>,
        ttl: Duration,
    ) -> Self {
        Self {
            network: rpc.network().into(),
            rpc,
            limits,
            tokens,
            holdings,
            prices: Cache::new(ttl),
        }
    }

    /// The prices: kept, else asked.
    ///
    /// # Errors
    ///
    /// The node's failure.
    pub(crate) async fn prices(&self) -> Result<Prices, TransactionError> {
        self.prices
            .get(|| async {
                self.rpc
                    .prices()
                    .await
                    .map_err(TransactionError::unavailable)
            })
            .await
    }

    /// Per spend, in order, whether its recipient holds its asset (for TRX:
    /// its account exists). One read.
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

    /// A token transfer's `fee_limit` at `rate`: [`FeeLimits::energy`] at
    /// its base, the energy price.
    ///
    /// # Errors
    ///
    /// `FeeCap` above [`FeeLimits::max_fee_limit`].
    pub(crate) fn fee_limit(&self, rate: FeeRate) -> Result<u64, TransactionError> {
        let limit = u128::from(self.limits.energy).saturating_mul(rate.base);
        u64::try_from(limit)
            .ok()
            .filter(|limit| *limit <= self.limits.max_fee_limit)
            .ok_or_else(|| {
                TransactionError::new(
                    TransactionErrorKind::FeeCap,
                    format!("Tron fee_limit {limit} sun exceeds the configured ceiling"),
                )
            })
    }

    /// The most a transfer burns: its bandwidth at `prices`, and a token's
    /// `fee_limit`, or for TRX the account it creates unless the recipient
    /// `held` it.
    pub(crate) const fn fee(prices: Prices, fee_limit: Option<u64>, held: bool) -> u64 {
        match fee_limit {
            Some(limit) => TOKEN_BYTES
                .saturating_mul(prices.bandwidth)
                .saturating_add(limit),
            None if held => TRX_BYTES.saturating_mul(prices.bandwidth),
            None => TRX_BYTES
                .saturating_mul(prices.bandwidth)
                .saturating_add(prices.account),
        }
    }
}

impl base::Networked for Estimator {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

/// The energy price is the rate's base; nothing is paid on top.
impl FeeRateProvider for Estimator {
    fn rate(&self) -> base::Future<'_, Result<FeeRate, TransactionError>> {
        Box::pin(async move {
            let prices = self.prices().await?;
            Ok(FeeRate {
                base: prices.energy.into(),
                tip: 0,
            })
        })
    }
}

impl base::FeeEstimator for Estimator {
    /// Σ per spend [`Estimator::fee`], each sender its own transaction:
    /// exactly the fee leg a builder signs.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let held = self.holds(spends).await?;
            let prices = self.prices().await?;
            let rate = self.rate().await?;
            let mut total = 0_u64;
            for (spend, held) in spends.iter().zip(held) {
                if spend.from.network != self.network {
                    return Err(anyhow!("not an address of this Tron network"));
                }
                let contract = self.tokens.contract(&spend.asset)?;
                let limit = contract.map(|_| self.fee_limit(rate)).transpose()?;
                let fee = Self::fee(prices, limit, held && spend.to.is_some());
                total = total.saturating_add(fee);
            }
            Ok(Decimal::from(total))
        })
    }
}
