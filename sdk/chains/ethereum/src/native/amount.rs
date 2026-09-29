use alloy_primitives::U256;
use base::Decimal;

use super::{ETH, Error, ErrorKind};

/// An unsigned 256-bit Ethereum amount in atomic units, big-endian.
///
/// Wei for ETH; the token's smallest unit for an ERC-20 amount.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Wei([u8; 32]);

impl Wei {
    /// No wei.
    pub const ZERO: Self = Self([0; 32]);

    /// `units` atomic units, big-endian.
    #[must_use]
    pub const fn from_atomic(units: [u8; 32]) -> Self {
        Self(units)
    }

    /// This amount in the chain's own atomic width, the exact inverse of
    /// [`Wei::from_atomic`].
    #[must_use]
    pub const fn units(&self) -> [u8; 32] {
        self.0
    }

    /// `value` wei.
    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(U256::from(value).to_be_bytes())
    }

    /// Converts a positive amount of an asset with `decimals` decimals (18
    /// for ETH) to atomic units, without rounding.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] for zero, a negative amount, more
    /// than `decimals` fractional digits, or more than 256 bits.
    pub fn from_decimal(value: &Decimal, decimals: u32) -> Result<Self, Error> {
        let amount = value
            .to_atomic_be_bytes(decimals)
            .map(Self)
            .map_err(|error| Error::new(ErrorKind::InvalidTransaction, error.message))?;
        if amount.is_zero() {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum amount must be positive",
            ));
        }
        Ok(amount)
    }

    /// This amount as a count of atomic units (0 decimals), as indexing
    /// stores it.
    #[must_use]
    pub fn atomic(&self) -> Decimal {
        self.decimal_with(0)
    }

    /// This amount as ETH (18 decimals).
    #[must_use]
    pub fn decimal(&self) -> Decimal {
        self.decimal_with(ETH.decimals)
    }

    /// This amount as a decimal with `decimals` fractional digits: an ERC-20
    /// amount is read in its own token's decimals, the twin of
    /// [`Wei::from_decimal`].
    #[must_use]
    pub fn decimal_with(&self, decimals: u32) -> Decimal {
        Decimal::from_atomic(num_bigint::BigUint::from_bytes_be(&self.0), decimals)
    }

    /// Whether this amount is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }

    /// This amount as `u128`, if it fits.
    #[must_use]
    pub fn checked_to_u128(&self) -> Option<u128> {
        u128::try_from(self.u256()).ok()
    }

    /// `self + other`, or `None` on 256-bit overflow.
    #[must_use]
    pub fn checked_add(&self, other: &Self) -> Option<Self> {
        self.u256().checked_add(other.u256()).map(Self::from_u256)
    }

    /// `self - other`, or `None` below zero.
    #[must_use]
    pub fn checked_sub(&self, other: &Self) -> Option<Self> {
        self.u256().checked_sub(other.u256()).map(Self::from_u256)
    }

    /// `self * multiplier`, or `None` on 256-bit overflow.
    #[must_use]
    pub fn checked_mul_u64(&self, multiplier: u64) -> Option<Self> {
        self.u256()
            .checked_mul(U256::from(multiplier))
            .map(Self::from_u256)
    }

    pub(crate) fn u256(&self) -> U256 {
        U256::from_be_bytes(self.0)
    }

    pub(crate) fn from_u256(value: U256) -> Self {
        Self(value.to_be_bytes())
    }
}

impl std::fmt::Display for Wei {
    /// Atomic units in base 10.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.u256())
    }
}
