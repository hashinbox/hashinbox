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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn converts_exact_decimal_boundaries_without_floating_point() {
        let cases = [
            ("1", 1_000_000_000_u64, "1"),
            ("0.000000001", 1, "0.000000001"),
            ("1.0000000000", 1_000_000_000, "1"),
            ("18446744073.709551615", u64::MAX, "18446744073.709551615"),
        ];

        for (input, atomic, rendered) in cases {
            let amount = Decimal::from_str(input).expect("fixture decimal must parse");
            let lamports =
                Lamport::from_decimal(&amount, SOL.decimals).expect("exact amount must convert");
            assert_eq!(lamports.units(), atomic);
            assert_eq!(lamports.decimal().to_string(), rendered);
        }
    }

    /// The scale is an argument in both directions, so a mint's amount is read
    /// in the mint's own units and never in SOL's nine. Before this argument
    /// existed, `WalletConfig::decimals` was a field no conversion read, and
    /// the first SPL send would have scaled a six-decimal amount by 10^9.
    #[test]
    fn a_mint_scale_is_read_in_its_own_units_in_both_directions() {
        let amount = Decimal::from_str("1.5").expect("fixture decimal must parse");
        let token = Lamport::from_decimal(&amount, 6).expect("a six-decimal mint amount");
        assert_eq!(token.units(), 1_500_000);
        assert_eq!(token.decimal_with(6), amount);

        let sol = Lamport::from_decimal(&amount, SOL.decimals).expect("the same amount in SOL");
        assert_eq!(sol.units(), 1_500_000_000);
        assert_eq!(sol.decimal(), amount);

        // The mint's own scale refuses what SOL's nine would have accepted.
        let precise = Decimal::from_str("0.0000001").expect("fixture decimal must parse");
        assert!(Lamport::from_decimal(&precise, SOL.decimals).is_ok());
        assert_eq!(
            Lamport::from_decimal(&precise, 6)
                .expect_err("more fractional digits than the mint has")
                .kind,
            ErrorKind::InvalidTransaction
        );
    }

    #[test]
    fn rejects_zero_negative_fractional_and_overflow_amounts() {
        for input in ["0", "-0.000000001", "0.0000000001", "18446744073.709551616"] {
            let amount = Decimal::from_str(input).expect("fixture decimal must parse");
            let error = Lamport::from_decimal(&amount, SOL.decimals)
                .expect_err("invalid native amount must fail");
            assert_eq!(error.kind, ErrorKind::InvalidTransaction, "{input}");
            assert!(!error.retryable);
        }
        let zero = Lamport::from_decimal(&Decimal::zero(), SOL.decimals).unwrap_err();
        assert_eq!(zero.message, "Solana amount must be greater than zero");
    }

    #[test]
    fn atomic_zero_is_valid_for_observed_balances() {
        assert_eq!(Lamport::ZERO.units(), 0);
        assert_eq!(Lamport::ZERO.decimal(), Decimal::zero());
        assert_eq!(Lamport::from_atomic(7).to_string(), "7");
    }

    #[test]
    fn a_mint_scale_renders_the_same_units_as_its_own_token() {
        let amount = Lamport::from_atomic(1_500_000);
        assert_eq!(amount.decimal_with(6).to_string(), "1.5");
        assert_eq!(amount.decimal_with(0).to_string(), "1500000");
        assert_eq!(amount.decimal_with(SOL.decimals), amount.decimal());
        assert_eq!(amount.atomic().to_string(), "1500000");
    }

    #[test]
    fn checked_arithmetic_never_wraps() {
        assert_eq!(
            Lamport::from_atomic(2).checked_add(Lamport::from_atomic(3)),
            Some(Lamport::from_atomic(5))
        );
        assert_eq!(
            Lamport::from_atomic(u64::MAX).checked_add(Lamport::from_atomic(1)),
            None
        );
        assert_eq!(
            Lamport::from_atomic(5).checked_sub(Lamport::from_atomic(3)),
            Some(Lamport::from_atomic(2))
        );
        assert_eq!(
            Lamport::from_atomic(0).checked_sub(Lamport::from_atomic(1)),
            None
        );
    }
}
