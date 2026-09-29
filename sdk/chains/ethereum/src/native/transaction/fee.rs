use crate::native::{Error, ErrorKind, Wei};

const BASIS_POINTS_DENOMINATOR: u64 = 10_000;

/// Explicit transaction-construction safety limits applied to RPC results.
///
/// The node remains the source of nonce, gas, and fee observations, but it
/// cannot cause the wallet to build a transaction above operator-selected
/// ceilings. A value over a ceiling is rejected rather than silently clamped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeeLimits {
    max_input_bytes: usize,
    gas_limit_margin_basis_points: u32,
    max_gas_limit: u64,
    max_fee_per_gas: Wei,
    max_priority_fee_per_gas: Wei,
    max_total_fee: Wei,
}

impl FeeLimits {
    /// Validates and stores the six ceilings.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for a zero ceiling, a margin above
    /// 100%, or a priority-fee ceiling above the max-fee ceiling.
    pub fn new(
        max_input_bytes: usize,
        gas_limit_margin_basis_points: u32,
        max_gas_limit: u64,
        max_fee_per_gas: Wei,
        max_priority_fee_per_gas: Wei,
        max_total_fee: Wei,
    ) -> Result<Self, Error> {
        if max_input_bytes == 0 {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum maximum transaction input size must be greater than zero",
            ));
        }
        if u64::from(gas_limit_margin_basis_points) > BASIS_POINTS_DENOMINATOR {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum gas-limit margin must not exceed 10000 basis points",
            ));
        }
        if max_gas_limit == 0 {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum maximum gas limit must be greater than zero",
            ));
        }
        if max_fee_per_gas.is_zero() || max_total_fee.is_zero() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum fee ceilings must be greater than zero",
            ));
        }
        if max_priority_fee_per_gas > max_fee_per_gas {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum priority-fee ceiling must not exceed the max-fee ceiling",
            ));
        }

        Ok(Self {
            max_input_bytes,
            gas_limit_margin_basis_points,
            max_gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            max_total_fee,
        })
    }

    pub(crate) fn gas_limit_with_margin(&self, estimated: u64) -> Result<u64, Error> {
        let numerator = u128::from(estimated)
            .checked_mul(u128::from(self.gas_limit_margin_basis_points))
            .ok_or_else(|| Error::invalid_response("eth_estimateGas", "gas margin overflowed"))?;
        let margin = numerator
            .checked_add(u128::from(BASIS_POINTS_DENOMINATOR - 1))
            .map(|value| value / u128::from(BASIS_POINTS_DENOMINATOR))
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| Error::invalid_response("eth_estimateGas", "gas margin exceeds u64"))?;
        estimated.checked_add(margin).ok_or_else(|| {
            Error::invalid_response("eth_estimateGas", "gas limit with margin exceeds u64")
        })
    }

    pub(crate) fn gas_limit(&self, estimated: u64) -> Result<u64, Error> {
        if estimated == 0 {
            return Err(Error::invalid_response(
                "eth_estimateGas",
                "estimated gas limit is zero",
            ));
        }
        let gas_limit = self.gas_limit_with_margin(estimated)?;
        if gas_limit > self.max_gas_limit {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Ethereum estimated gas limit exceeds the configured ceiling",
            ));
        }
        Ok(gas_limit)
    }

    pub(crate) fn validate_priority_fee(&self, fee: &Wei) -> Result<(), Error> {
        if fee > &self.max_priority_fee_per_gas {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Ethereum priority fee exceeds the configured ceiling",
            ));
        }
        Ok(())
    }

    pub(crate) fn max_fee(&self, base: &Wei, priority: &Wei) -> Result<Wei, Error> {
        let max_fee = base
            .checked_mul_u64(2)
            .and_then(|fee| fee.checked_add(priority))
            .ok_or_else(|| {
                Error::invalid_response("eth_getBlockByNumber", "fee calculation overflowed U256")
            })?;
        if max_fee > self.max_fee_per_gas {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Ethereum max fee per gas exceeds the configured ceiling",
            ));
        }
        if &max_fee < priority {
            return Err(Error::invalid_response(
                "eth_getBlockByNumber",
                "calculated max fee is below the priority fee",
            ));
        }
        Ok(max_fee)
    }

    pub(crate) fn total_fee(&self, gas_limit: u64, fee: &Wei) -> Result<Wei, Error> {
        let total_fee = fee.checked_mul_u64(gas_limit).ok_or_else(|| {
            Error::new(
                ErrorKind::FeeUnavailable,
                "Ethereum maximum transaction fee overflowed U256",
            )
        })?;
        if total_fee > self.max_total_fee {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Ethereum maximum transaction fee exceeds the configured ceiling",
            ));
        }
        Ok(total_fee)
    }

    /// Largest calldata accepted, in bytes.
    #[must_use]
    pub const fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    /// Margin added to `eth_estimateGas`, in basis points.
    #[must_use]
    pub const fn gas_limit_margin_basis_points(&self) -> u32 {
        self.gas_limit_margin_basis_points
    }

    /// Largest gas limit accepted.
    #[must_use]
    pub const fn max_gas_limit(&self) -> u64 {
        self.max_gas_limit
    }

    /// Largest max fee per gas accepted.
    #[must_use]
    pub const fn max_fee_per_gas(&self) -> &Wei {
        &self.max_fee_per_gas
    }

    /// Largest priority fee per gas accepted.
    #[must_use]
    pub const fn max_priority_fee_per_gas(&self) -> &Wei {
        &self.max_priority_fee_per_gas
    }

    /// Largest worst-case fee (`gas limit * max fee`) accepted.
    #[must_use]
    pub const fn max_total_fee(&self) -> &Wei {
        &self.max_total_fee
    }
}

impl FeeLimits {
    /// Default ceiling on ERC-20 or native calldata: 128 KiB.
    pub const MAX_INPUT_BYTES: usize = 128 * 1024;
    /// Default margin added to `eth_estimateGas`: 20%.
    pub const GAS_LIMIT_MARGIN_BASIS_POINTS: u32 = 2_000;
    /// Default gas-limit ceiling: one mainnet block (30M gas).
    pub const MAX_GAS_LIMIT: u64 = 30_000_000;
    /// Default max fee per gas: 1,000 gwei.
    pub const MAX_FEE_PER_GAS: u128 = 1_000_000_000_000;
    /// Default max priority fee per gas: 100 gwei.
    pub const MAX_PRIORITY_FEE_PER_GAS: u128 = 100_000_000_000;
    /// Default ceiling on one transaction's worst-case fee: 10 ETH.
    pub const MAX_TOTAL_FEE: u128 = 10_000_000_000_000_000_000;
}

impl Default for FeeLimits {
    /// The documented constants above; valid by construction.
    fn default() -> Self {
        Self {
            max_input_bytes: Self::MAX_INPUT_BYTES,
            gas_limit_margin_basis_points: Self::GAS_LIMIT_MARGIN_BASIS_POINTS,
            max_gas_limit: Self::MAX_GAS_LIMIT,
            max_fee_per_gas: Wei::from_u128(Self::MAX_FEE_PER_GAS),
            max_priority_fee_per_gas: Wei::from_u128(Self::MAX_PRIORITY_FEE_PER_GAS),
            max_total_fee: Wei::from_u128(Self::MAX_TOTAL_FEE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> FeeLimits {
        FeeLimits::new(
            1024,
            2_000,
            1_000_000,
            Wei::from_u128(1_000_000_000_000),
            Wei::from_u128(100_000_000_000),
            Wei::from_u128(1_000_000_000_000_000_000),
        )
        .expect("test limits must be valid")
    }

    #[test]
    fn default_limits_pass_their_own_validation() {
        let default = FeeLimits::default();
        let validated = FeeLimits::new(
            default.max_input_bytes(),
            default.gas_limit_margin_basis_points(),
            default.max_gas_limit(),
            default.max_fee_per_gas().clone(),
            default.max_priority_fee_per_gas().clone(),
            default.max_total_fee().clone(),
        )
        .expect("defaults must be valid");
        assert_eq!(validated, default);
        let error =
            FeeLimits::new(0, 0, 1, Wei::from_u128(1), Wei::ZERO, Wei::from_u128(1)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
    }

    #[test]
    fn configured_gas_margin_rounds_up_without_losing_large_values() {
        for (basis_points, estimated, expected) in [
            (0, u64::MAX, u64::MAX),
            (1, 1, 2),
            (2_000, 21_000, 25_200),
            (2_000, 21_001, 25_202),
            (10_000, u64::MAX / 2, u64::MAX - 1),
        ] {
            let mut limits = limits();
            limits.gas_limit_margin_basis_points = basis_points;
            assert_eq!(limits.gas_limit_with_margin(estimated).unwrap(), expected);
        }
    }

    #[test]
    fn gas_margin_overflow_remains_a_terminal_rpc_response_error() {
        let error = limits()
            .gas_limit_with_margin(u64::MAX)
            .expect_err("adding the configured margin must not wrap");
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "Ethereum RPC eth_estimateGas returned an invalid response: gas limit with \
             margin exceeds u64"
        );
    }
}
