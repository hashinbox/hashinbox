//! [`Estimator`]: Ethereum's `base::FeeEstimator`, asking the node and
//! keeping its answers, by Ethereum's rules, so the node is asked once per
//! kind of send per time to live, however many wallets send.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use alloy_consensus::TxEip1559;
use alloy_primitives::{Bytes, TxKind, U256};
use anyhow::Context as _;
use base::{Cache, Decimal, FeeRateProvider, Holdings, Reason, Spend, Spends, TransactionError};
use num_bigint::BigUint;

use crate::{Address, Client, Tokens, erc20, fee::FeeLimits};

/// A recipient no one has used: a fresh pseudo-random address per call, so
/// an estimate against it includes creating the recipient (its account for
/// ether, its balance slot for a token), the dearest case. A fixed address
/// would not do: anyone can fund it, and estimates against it would then
/// leave out the creation.
fn unused() -> alloy_primitives::Address {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut seed = nanos.to_be_bytes().to_vec();
    seed.extend(CALLS.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    alloy_primitives::Address::from_slice(&alloy_primitives::keccak256(seed)[12..])
}

/// Ethereum's `base::FeeEstimator`, which every builder of the factory
/// signs with too, so an estimate and the transaction signed after it
/// agree. It asks the node and keeps its answers for `ttl`. On Ethereum the
/// price per gas unit is the same for every send (the rate of `rates`), and
/// a transfer's gas depends only on its asset and on whether its recipient
/// holds the asset already (nothing to create), not on who sends it or to
/// which such recipient. So it keeps the gas of each (asset, recipient
/// holds it), filled by the first spend of that kind to miss.
pub(crate) struct Estimator {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) limits: FeeLimits,
    pub(crate) tokens: Tokens,
    pub(crate) rates: Arc<dyn FeeRateProvider>,
    /// Who holds what: which kind each spend is.
    pub(crate) holdings: Arc<dyn Holdings>,
    gas: BTreeMap<(Option<alloy_primitives::Address>, bool), Cache<u64>>,
}

impl Estimator {
    /// Over `rpc`, sending ether and `tokens` under `limits` at the rate of
    /// `rates`, its answers kept `ttl`, kinds of send as `holdings` say.
    pub(crate) fn new(
        rpc: Client,
        limits: FeeLimits,
        tokens: Tokens,
        rates: Arc<dyn FeeRateProvider>,
        holdings: Arc<dyn Holdings>,
        ttl: Duration,
    ) -> Self {
        let gas = std::iter::once(None)
            .chain(tokens.iter().map(Some))
            .flat_map(|contract| [(contract, true), (contract, false)])
            .map(|kind| (kind, Cache::new(ttl)))
            .collect();
        Self {
            network: rpc.network().into(),
            rpc,
            limits,
            tokens,
            rates,
            holdings,
            gas,
        }
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
    /// `held`: kept for its kind, else the node's estimate of that very
    /// transfer (to a fresh unused address without a recipient: the dearest
    /// case) plus the margin, one call, as if the sender held ample ether
    /// (the gas does not depend on it). A sender holding none of the token
    /// cannot be asked for (the node runs the transfer), so it gets the
    /// kind's last answer however old: the sender does not change the gas.
    ///
    /// # Errors
    ///
    /// An asset that is neither ether nor an allowlisted token, an address
    /// of another network, or the node's failure with no earlier answer to
    /// fall back on.
    pub(crate) async fn gas(&self, spend: &Spend, held: bool) -> Result<u64, TransactionError> {
        let contract = self.tokens.contract(&spend.asset)?;
        let held = held && spend.to.is_some();
        let Some(cache) = self.gas.get(&(contract, held)) else {
            return Err(TransactionError::failed("no gas kept for this asset"));
        };
        let ask = || async {
            let native = self.rpc.network();
            let from =
                Address::parse(&spend.from.value, native).map_err(TransactionError::failed)?;
            let to = match &spend.to {
                Some(to) => *Address::parse(&to.value, native)
                    .map_err(TransactionError::failed)?
                    .as_native(),
                None => unused(),
            };
            let request = match contract {
                None => TxEip1559 {
                    to: TxKind::Call(to),
                    value: U256::from(1),
                    ..TxEip1559::default()
                },
                Some(contract) => TxEip1559 {
                    to: TxKind::Call(contract),
                    input: Bytes::from(erc20::transfer(to, U256::from(1))),
                    ..TxEip1559::default()
                },
            };
            let estimated = self.rpc.estimate(&from, &request).await?;
            self.limits
                .gas_limit(estimated)
                .map_err(TransactionError::failed)
        };
        match cache.get(ask).await {
            Err(error) if error.reason == Some(Reason::Reverted) => cache.last().await.ok_or(error),
            answer => answer,
        }
    }
}

impl base::Networked for Estimator {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl base::FeeEstimator for Estimator {
    /// Σ per spend `gas limit × max fee per gas`, each sender its own
    /// transaction, each under `limits`' ceiling: exactly the fee leg a
    /// builder signs.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let held = self.holds(spends).await?;
            let fees = self.rates.rate().await?;
            let mut gas = Vec::with_capacity(spends.len());
            for (spend, held) in spends.iter().zip(held) {
                gas.push(self.gas(spend, held).await?);
            }
            let mut total = U256::ZERO;
            for gas in gas {
                total = total
                    .checked_add(self.limits.total(gas, fees)?)
                    .context("Ethereum fee estimate overflows")?;
            }
            Ok(Decimal::from_atomic(
                BigUint::from_bytes_be(&total.to_be_bytes::<32>()),
                0,
            ))
        })
    }
}
