#![doc = include_str!("../README.md")]

mod error;
mod notation;

pub use error::Error;

use bigdecimal::{BigDecimal, Zero};
use num_bigint::{BigInt, BigUint, Sign};

/// An exact, arbitrary-precision base-10 value.
///
/// The value is `coefficient * 10^-scale` with a non-negative scale, kept
/// canonical: trailing fractional zeros are dropped, so equal values have one
/// spelling and its `Display` output round-trips through
/// [`str::parse`](str::parse). No binary floating point is involved anywhere,
/// so money stays exact regardless of magnitude or fractional digits.
///
/// Arithmetic is deliberately narrow. This type carries amounts between a
/// chain, storage, and an HTTP boundary, and the one operation a ledger
/// cannot express without it is the total of many amounts of one asset
/// ([`Decimal::total`]). It never subtracts, divides, or rounds: a balance
/// that needs a difference reports its two sides instead.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Decimal(pub(crate) BigDecimal);

impl Decimal {
    /// Longest accepted decimal text, in bytes.
    ///
    /// The widest amount any supported chain can express is `2^256 - 1`
    /// atomic units, which is 78 digits, and the widest `decimals` an ERC-20
    /// or SPL token can declare is 255, because both encode it in one byte.
    /// The longest legitimate amount is therefore about 335 characters
    /// (`0.` + 255 fractional digits, or 78 whole digits plus a point and
    /// 255 fractional digits). This bound doubles that headroom while keeping
    /// every parse, comparison, and conversion linear in a few hundred
    /// digits, so an oversized request body cannot turn into a
    /// million-digit rescale.
    pub const MAX_LEN: usize = 512;

    /// Exact zero.
    #[must_use]
    pub fn zero() -> Self {
        Self(BigDecimal::zero())
    }

    /// Display amount of `units` atomic units of an asset with `decimals`
    /// fractional digits.
    ///
    /// `decimals` is asset metadata chosen at composition, not caller input;
    /// [`Decimal::MAX_LEN`] bounds the untrusted entry point,
    /// [`str::parse`](str::parse).
    #[must_use]
    pub fn from_atomic(units: BigUint, decimals: u32) -> Self {
        Self::canonical(BigDecimal::new(BigInt::from(units), i64::from(decimals)))
    }

    /// The canonical number of fractional digits.
    #[must_use]
    pub fn scale(&self) -> u32 {
        u32::try_from(self.0.fractional_digit_count()).unwrap_or_default()
    }

    /// Whether the value is exactly zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// Validates the invariant shared by monetary amounts: they may be zero,
    /// but never negative.
    ///
    /// # Errors
    ///
    /// Fails for a value below zero.
    pub fn validate_amount(&self) -> Result<(), Error> {
        if self.0.sign() == Sign::Minus {
            return Err(Error::negative());
        }
        Ok(())
    }

    /// Atomic units of an asset with `decimals` fractional digits.
    ///
    /// # Errors
    ///
    /// Fails for a negative amount, or one carrying more than `decimals`
    /// fractional digits, which would have to be rounded away.
    pub fn to_atomic(&self, decimals: u32) -> Result<BigUint, Error> {
        self.validate_amount()?;
        let units = self.0.with_scale(i64::from(decimals));
        if units != self.0 {
            return Err(Error::new(format!(
                "amount has more than {decimals} fractional digits"
            )));
        }
        BigUint::try_from(units.into_bigint_and_scale().0).map_err(|_| Error::negative())
    }

    /// [`Decimal::to_atomic`] narrowed to `u64` (satoshis, lamports).
    ///
    /// # Errors
    ///
    /// As [`Decimal::to_atomic`], plus a value beyond the `u64` range.
    pub fn to_atomic_u64(&self, decimals: u32) -> Result<u64, Error> {
        u64::try_from(self.to_atomic(decimals)?)
            .map_err(|_| Error::new("atomic amount exceeds the u64 range"))
    }

    /// [`Decimal::to_atomic`] as a fixed-width big-endian integer (for
    /// example a 32-byte EVM word).
    ///
    /// # Errors
    ///
    /// As [`Decimal::to_atomic`], plus a value wider than `N` bytes.
    pub fn to_atomic_be_bytes<const N: usize>(&self, decimals: u32) -> Result<[u8; N], Error> {
        let bytes = self.to_atomic(decimals)?.to_bytes_be();
        if bytes.len() > N {
            return Err(Error::new(format!("atomic amount exceeds {N} bytes")));
        }
        let mut value = [0_u8; N];
        value[N - bytes.len()..].copy_from_slice(&bytes);
        Ok(value)
    }

    /// The exact total of `amounts`.
    ///
    /// Addition is exact and loses no precision at any scale, and a total of
    /// non-negative amounts is non-negative, so this is the one arithmetic a
    /// ledger fold needs. It is deliberately not `Add`: a total is a fold over
    /// a set of amounts of one asset, not a general operator on two values of
    /// unrelated ones.
    #[must_use]
    pub fn total<'a>(amounts: impl IntoIterator<Item = &'a Self>) -> Self {
        Self::canonical(
            amounts
                .into_iter()
                .fold(BigDecimal::zero(), |sum, amount| sum + &amount.0),
        )
    }

    /// Drops trailing fractional zeros and keeps the scale non-negative, so
    /// one value has one representation and one spelling.
    pub(crate) fn canonical(value: BigDecimal) -> Self {
        let normalized = value.normalized();
        if normalized.fractional_digit_count() < 0 {
            return Self(normalized.with_scale(0));
        }
        Self(normalized)
    }
}

impl From<u64> for Decimal {
    fn from(value: u64) -> Self {
        Self::canonical(BigDecimal::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Decimal {
        text.parse::<Decimal>().expect("canonical decimal notation")
    }

    #[test]
    fn converts_human_units_to_exact_atomic_units() {
        assert_eq!(parse("1").to_atomic_u64(8).expect("satoshis"), 100_000_000);
        assert_eq!(
            parse("1").to_atomic(18).expect("wei").to_string(),
            "1000000000000000000"
        );
        assert_eq!(parse("0.00000001").to_atomic_u64(8).expect("satoshi"), 1);
    }

    #[test]
    fn preserves_unbounded_magnitude_and_exact_display() {
        let text = "123456789012345678901234567890.000000000000000001";
        assert_eq!(parse(text).to_string(), text);
    }

    #[test]
    fn rejects_negative_currency_and_excess_precision() {
        assert_eq!(
            parse("-1").to_atomic(8).expect_err("negative").message,
            "currency amount must not be negative"
        );
        assert_eq!(
            parse("0.000000001")
                .to_atomic(8)
                .expect_err("excess precision")
                .message,
            "amount has more than 8 fractional digits"
        );
    }

    #[test]
    fn a_total_is_exact_at_every_scale_and_empty_totals_are_zero() {
        assert!(Decimal::total(&[]).is_zero());
        assert_eq!(
            Decimal::total(&[parse("0.1"), parse("0.2")]).to_string(),
            "0.3"
        );
        let wide = [
            parse("123456789012345678901234567890"),
            parse("0.000000000000000001"),
        ];
        assert_eq!(
            Decimal::total(&wide).to_string(),
            "123456789012345678901234567890.000000000000000001"
        );
        assert_eq!(
            Decimal::total(&[parse("1.50"), parse("0.50")]).to_string(),
            "2"
        );
        assert_eq!(Decimal::total(&[Decimal::zero()]).scale(), 0);
    }

    #[test]
    fn zero_is_canonical_and_nonnegative() {
        let zero = Decimal::zero();
        assert!(zero.is_zero());
        assert_eq!(zero.scale(), 0);
        assert_eq!(zero.to_string(), "0");
        assert_eq!(zero.validate_amount(), Ok(()));
        assert_eq!(zero.to_atomic(18).expect("zero wei"), BigUint::ZERO);
    }

    #[test]
    fn narrowing_conversions_report_the_width_they_exceed() {
        let maximum = BigUint::from_bytes_be(&[0xff; 32]);
        let widest = Decimal::from_atomic(maximum.clone(), 0);
        assert_eq!(widest.to_atomic(0).expect("256-bit units"), maximum);
        assert_eq!(
            widest.to_atomic_be_bytes::<32>(0).expect("EVM word"),
            [0xff; 32]
        );
        assert_eq!(
            widest.to_atomic_u64(0).expect_err("too wide").message,
            "atomic amount exceeds the u64 range"
        );
        assert_eq!(
            widest
                .to_atomic_be_bytes::<8>(0)
                .expect_err("too wide")
                .message,
            "atomic amount exceeds 8 bytes"
        );
    }

    #[test]
    fn maximum_ethereum_amount_round_trips_through_text_at_every_scale() {
        let maximum = BigUint::from_bytes_be(&[0xff; 32]);
        for decimals in [0, 18] {
            let value = Decimal::from_atomic(maximum.clone(), decimals);
            assert_eq!(parse(&value.to_string()), value);
            assert_eq!(value.to_atomic(decimals).expect("atomic units"), maximum);
        }
        assert_eq!(
            Decimal::from_atomic(maximum, 18).to_string(),
            "115792089237316195423570985008687907853269984665640564039457.584007913129639935"
        );
    }

    #[test]
    fn ordering_compares_numeric_values_instead_of_structural_parts() {
        assert!(parse("0.00059859") < parse("0.0006"));
        assert!(parse("0.0006") > parse("0.00059859"));
        assert_eq!(
            parse("1.2300"),
            Decimal::from_atomic(BigUint::from(123_u64), 2)
        );
    }

    #[test]
    fn ordering_is_antisymmetric_across_zero_and_the_widest_scale() {
        let widest = format!("0.{}1", "0".repeat(Decimal::MAX_LEN - 4));
        let values = [
            parse(&format!("-{}", "9".repeat(50))),
            parse("-1"),
            parse("-0.0006"),
            parse("-0.00059859"),
            parse(&format!("-{widest}")),
            Decimal::zero(),
            parse(&widest),
            parse("0.00059859"),
            parse("0.0006"),
            Decimal::from(1_u64),
            parse(&"9".repeat(50)),
        ];
        assert_eq!(values[5].scale(), 0);
        for (left_index, left) in values.iter().enumerate() {
            for (right_index, right) in values.iter().enumerate() {
                assert_eq!(left.cmp(right), left_index.cmp(&right_index));
                assert_eq!(left.partial_cmp(right), Some(left_index.cmp(&right_index)));
            }
        }
    }

    #[test]
    fn equal_values_share_one_representation_and_one_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let left = parse("1.2300");
        let right = Decimal::from_atomic(BigUint::from(1_230_u64), 3);
        assert_eq!(left, right);
        assert_eq!(left.scale(), right.scale());
        assert_eq!(left.to_string(), right.to_string());

        let hash = |value: &Decimal| {
            let mut hasher = DefaultHasher::new();
            value.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&left), hash(&right));
    }

    #[test]
    fn whole_numbers_keep_their_trailing_zeros() {
        assert_eq!(Decimal::from(1_000_u64).to_string(), "1000");
        assert_eq!(Decimal::from(1_000_u64).scale(), 0);
        assert_eq!(
            Decimal::from_atomic(BigUint::from(100_u64), 0).to_string(),
            "100"
        );
    }
}
