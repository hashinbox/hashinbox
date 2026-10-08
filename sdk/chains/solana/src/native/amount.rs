use std::fmt;

use base::Decimal;

use super::{Error, ErrorKind, SOL};

/// An exact count of a Solana asset's atomic units.
///
/// For native SOL that unit is the lamport (10^-9 SOL), which names the type
/// and fixes [`Lamport::decimal`] to nine decimals. An SPL token amount is the
/// same 64-bit count in its mint's own units; the mint names the scale, so a
/// token amount is rendered with [`Lamport::decimal_with`] and never with
/// [`Lamport::decimal`].
///
/// The scale is named in **both** directions, exactly as `Wei` names it:
/// [`Lamport::from_decimal`] takes the `decimals` of the asset it is
/// converting, so a mint's amount can never be read in SOL's nine by default
/// (`docs/DEFECT_PATTERNS.md` section 1.1 — the value the type does not read
/// is the value it should not silently assume either).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lamport(u64);

impl Lamport {
    /// No lamports.
    pub const ZERO: Self = Self(0);

    /// `lamports` lamports.
    #[must_use]
    pub const fn from_atomic(lamports: u64) -> Self {
        Self(lamports)
    }

    /// Converts a positive amount of an asset with `decimals` decimals (nine
    /// for SOL, a mint's own for an SPL token) without rounding or truncation.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] for zero, a negative amount, more
    /// than `decimals` fractional digits, or more than `u64::MAX` atomic
    /// units.
    pub fn from_decimal(amount: &Decimal, decimals: u32) -> Result<Self, Error> {
        let value = amount
            .to_atomic_u64(decimals)
            .map_err(|error| Error::new(ErrorKind::InvalidTransaction, error.message))?;
        if value == 0 {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "Solana amount must be greater than zero",
            ));
        }
        Ok(Self(value))
    }

    /// This amount in the chain's own atomic width, the exact inverse of
    /// [`Lamport::from_atomic`].
    #[must_use]
    pub const fn units(self) -> u64 {
        self.0
    }

    /// This amount as a count of lamports (0 decimals), as indexing stores it.
    #[must_use]
    pub fn atomic(self) -> Decimal {
        Decimal::from(self.0)
    }

    /// This amount in SOL (9 decimals).
    #[must_use]
    pub fn decimal(self) -> Decimal {
        self.decimal_with(SOL.decimals)
    }

    /// This amount in the display units of an asset with `decimals` decimals,
    /// which a mint names for its own token.
    #[must_use]
    pub fn decimal_with(self, decimals: u32) -> Decimal {
        Decimal::from_atomic(self.0.into(), decimals)
    }

    /// `self + other`, or `None` on overflow.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// `self - other`, or `None` below zero.
    #[must_use]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }
}

impl fmt::Display for Lamport {
    /// The lamport count.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
