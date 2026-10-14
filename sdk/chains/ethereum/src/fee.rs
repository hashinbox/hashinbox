//! [`FeeLimits`]: ceilings on what a node's quotes may make a transaction
//! cost, and the EIP-1559 [`Fees`] built under them.

use alloy_primitives::U256;
use anyhow::{Context as _, ensure};
use serde::Deserialize;

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
