//! [`FeeLimits`]: ceilings on what a node's quotes may make a transaction
//! cost, and the EIP-1559 [`Fees`] built under them.

use alloy_consensus::TxEip1559;
use alloy_primitives::{Bytes, TxKind, U256};
use anyhow::{Context as _, ensure};
use base::{Asset, Decimal, TransactionError, TransactionErrorKind};
use num_bigint::BigUint;
use serde::Deserialize;

use crate::{Address, Client, Tokens, erc20};

/// A basis point of 10 000.
const BASIS_POINTS: u64 = 10_000;

/// Ceilings on what the node's gas and fee quotes may make a transaction
/// cost. A quote above a ceiling is refused, never clamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeeLimits {
    /// Margin added to `eth_estimateGas`, in basis points.
    pub gas_margin_basis_points: u32,
    /// Largest gas limit.
    pub max_gas_limit: u64,
    /// Largest max fee per gas, in wei.
    pub max_fee_per_gas: u128,
    /// Largest priority fee per gas, in wei.
    pub max_priority_fee_per_gas: u128,
    /// Largest worst-case fee (`gas limit * max fee`), in wei.
    pub max_total_fee: u128,
}

/// The EIP-1559 fees one transaction signs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fees {
    /// `2 * base fee + priority fee`.
    pub max_fee_per_gas: u128,
    /// The node's suggested tip.
    pub max_priority_fee_per_gas: u128,
}

impl Default for FeeLimits {
    /// 20% gas margin, one mainnet block (30M gas), 1 000 gwei max fee,
    /// 100 gwei tip, 10 ETH per transaction.
    fn default() -> Self {
        Self {
            gas_margin_basis_points: 2_000,
            max_gas_limit: 30_000_000,
            max_fee_per_gas: 1_000_000_000_000,
            max_priority_fee_per_gas: 100_000_000_000,
            max_total_fee: 10_000_000_000_000_000_000,
        }
    }
}

impl FeeLimits {
    /// Rejects ceilings that contradict each other.
    ///
    /// # Errors
    ///
    /// For a zero ceiling, a margin above 100%, or a tip ceiling above the
    /// max-fee ceiling.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            u64::from(self.gas_margin_basis_points) <= BASIS_POINTS,
            "Ethereum gas margin must not exceed 10000 basis points"
        );
        ensure!(
            self.max_gas_limit != 0 && self.max_fee_per_gas != 0 && self.max_total_fee != 0,
            "Ethereum fee ceilings must be greater than zero"
        );
        ensure!(
            self.max_priority_fee_per_gas <= self.max_fee_per_gas,
            "Ethereum tip ceiling must not exceed the max-fee ceiling"
        );
        Ok(())
    }

    /// `estimated` gas plus the margin, rounded up, within the ceiling.
    ///
    /// # Errors
    ///
    /// For a zero estimate or one above the ceiling.
    pub fn gas_limit(&self, estimated: u64) -> anyhow::Result<u64> {
        ensure!(estimated != 0, "Ethereum gas estimate is zero");
        let margin = u128::from(estimated) * u128::from(self.gas_margin_basis_points);
        let margin = margin.div_ceil(u128::from(BASIS_POINTS));
        let limit = u128::from(estimated) + margin;
        ensure!(
            limit <= u128::from(self.max_gas_limit),
            "Ethereum gas limit {limit} exceeds the configured ceiling"
        );
        u64::try_from(limit).context("Ethereum gas limit exceeds u64")
    }

    /// The fees of a block with `base` fee and a `priority` tip, within the
    /// ceilings.
    ///
    /// # Errors
    ///
    /// When a fee exceeds its ceiling.
    pub fn fees(&self, base: U256, priority: U256) -> anyhow::Result<Fees> {
        let max_priority_fee_per_gas = u128::try_from(priority)
            .ok()
            .filter(|tip| *tip <= self.max_priority_fee_per_gas)
            .context("Ethereum priority fee exceeds the configured ceiling")?;
        let max_fee_per_gas = base
            .checked_mul(U256::from(2))
            .and_then(|fee| fee.checked_add(priority))
            .and_then(|fee| u128::try_from(fee).ok())
            .filter(|fee| *fee <= self.max_fee_per_gas)
            .context("Ethereum max fee per gas exceeds the configured ceiling")?;
        Ok(Fees {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        })
    }

    /// `gas_limit * fees.max_fee_per_gas`, the most the transaction can cost,
    /// within the ceiling.
    ///
    /// # Errors
    ///
    /// When it exceeds the ceiling.
    pub fn total(&self, gas_limit: u64, fees: Fees) -> anyhow::Result<U256> {
        let total = U256::from(fees.max_fee_per_gas) * U256::from(gas_limit);
        ensure!(
            total <= U256::from(self.max_total_fee),
            "Ethereum worst-case fee exceeds the configured ceiling"
        );
        Ok(total)
    }
}

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

/// Ethereum's precise `base::FeeEstimator`: asks the node every time,
/// exactly for the senders, assets and recipients given; keeps nothing.
/// [`crate::fee_cache::Cached`] decides when its answers can be reused.
pub(crate) struct Estimator {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) limits: FeeLimits,
    pub(crate) tokens: Tokens,
}

impl Estimator {
    /// Over `rpc`, sending ether and `tokens` under `limits`.
    pub(crate) fn new(rpc: Client, limits: FeeLimits, tokens: Tokens) -> Self {
        Self {
            network: rpc.network().into(),
            rpc,
            limits,
            tokens,
        }
    }

    /// The price per gas unit now: the node's tip and the latest base fee,
    /// under `limits`. Two calls.
    ///
    /// # Errors
    ///
    /// The node's failure, or a quote above a ceiling.
    pub(crate) async fn fees(&self) -> Result<Fees, TransactionError> {
        let (base, priority) = self
            .rpc
            .fees()
            .await
            .map_err(TransactionError::unavailable)?;
        self.limits
            .fees(base, priority)
            .map_err(TransactionError::failed)
    }

    /// The gas limit of `spend`: the node's estimate of that very transfer
    /// (to a fresh unused address without a recipient: the dearest case)
    /// plus the margin. One call. As if the sender held ample ether: the gas
    /// does not depend on it. A token sender must hold the token: the node
    /// runs the transfer.
    ///
    /// # Errors
    ///
    /// An asset that is neither ether nor an allowlisted token, an address
    /// of another network, or the node's failure (`Reason::Reverted` for a
    /// sender holding none of the token).
    pub(crate) async fn gas(&self, spend: &base::Spend) -> Result<u64, TransactionError> {
        let native = self.rpc.network();
        let from = Address::parse(&spend.from.value, native).map_err(TransactionError::failed)?;
        let to = match &spend.to {
            Some(to) => *Address::parse(&to.value, native)
                .map_err(TransactionError::failed)?
                .as_native(),
            None => unused(),
        };
        let request = match self.contract(&spend.asset)? {
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
    }

    /// The contract of `asset`: `None` for ether.
    ///
    /// # Errors
    ///
    /// `Unsupported` for neither ether nor an allowlisted token of this
    /// network.
    pub(crate) fn contract(
        &self,
        asset: &Asset,
    ) -> Result<Option<alloy_primitives::Address>, TransactionError> {
        if *asset == Asset::native(self.network.clone()) {
            return Ok(None);
        }
        Some(asset)
            .filter(|asset| asset.network == self.network)
            .and_then(|asset| self.tokens.get(asset))
            .map(Some)
            .ok_or_else(|| {
                TransactionError::new(
                    TransactionErrorKind::Unsupported,
                    "not ether or an allowlisted token of this Ethereum network",
                )
            })
    }
}

impl base::Networked for Estimator {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl base::FeeEstimator for Estimator {
    /// Σ per spend `gas limit × max fee per gas`, each sender its own
    /// transaction, all asked from the node now.
    fn estimate<'a>(
        &'a self,
        spends: &'a base::Spends,
    ) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let fees = self.fees().await?;
            let mut gas = Vec::with_capacity(spends.len());
            for spend in spends.iter() {
                gas.push(self.gas(spend).await?);
            }
            total(&self.limits, fees, gas)
        })
    }
}

/// Σ `gas × max fee per gas` over `gas`, in wei, each under `limits`'
/// ceiling.
///
/// # Errors
///
/// A transaction above the ceiling, or an overflow.
pub(crate) fn total(
    limits: &FeeLimits,
    fees: Fees,
    gas: impl IntoIterator<Item = u64>,
) -> anyhow::Result<Decimal> {
    let mut total = U256::ZERO;
    for gas in gas {
        total = total
            .checked_add(limits.total(gas, fees)?)
            .context("Ethereum fee estimate overflows")?;
    }
    Ok(Decimal::from_atomic(
        BigUint::from_bytes_be(&total.to_be_bytes::<32>()),
        0,
    ))
}
