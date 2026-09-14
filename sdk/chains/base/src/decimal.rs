use std::{cmp::Ordering, error::Error, fmt, str::FromStr};

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{Signed, Zero};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalErrorKind {
    Invalid,
    NegativeAmount,
    ExcessPrecision,
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalError {
    pub kind: DecimalErrorKind,
    pub message: String,
}

/// Sign component used by the stable, lossless [`DecimalParts`] representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalSign {
    Positive,
    Negative,
}

/// Canonical components for persistence without converting through text or
/// binary floating point.
///
/// `magnitude` is an unsigned, minimal-width, big-endian integer. Zero has an
/// empty magnitude and must use [`DecimalSign::Positive`]. The represented
/// value is `sign * magnitude * 10^-scale`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalParts {
    pub sign: DecimalSign,
    pub magnitude: Vec<u8>,
    pub scale: u32,
}

impl DecimalError {
    #[must_use]
    pub fn new(kind: DecimalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid() -> Self {
        Self::new(
            DecimalErrorKind::Invalid,
            "decimal must use canonical base-10 notation",
        )
    }
}

impl fmt::Display for DecimalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DecimalError {}

/// Arbitrary-precision, base-10 fixed-point value.
///
/// No binary floating-point conversion is provided. Currency values therefore
/// remain exact regardless of their magnitude or number of fractional digits.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Decimal {
    coefficient: BigInt,
    scale: u32,
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.coefficient.sign(), other.coefficient.sign()) {
            (Sign::Minus, Sign::Minus) => other.compare_magnitude(self),
            (Sign::Minus, _) => Ordering::Less,
            (_, Sign::Minus) => Ordering::Greater,
            _ => self.compare_magnitude(other),
        }
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Decimal {
    #[must_use]
    pub fn zero() -> Self {
        Self::new(BigInt::ZERO, 0)
    }

    #[must_use]
    pub fn new(coefficient: BigInt, scale: u32) -> Self {
        Self::normalize(coefficient, scale)
    }

    #[must_use]
    pub fn from_atomic(units: BigUint, decimals: u32) -> Self {
        Self::new(BigInt::from(units), decimals)
    }

    #[must_use]
    pub fn coefficient(&self) -> &BigInt {
        &self.coefficient
    }

    #[must_use]
    pub const fn scale(&self) -> u32 {
        self.scale
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.coefficient.is_zero()
    }

    /// Returns canonical, lossless components suitable for a versioned
    /// persistence format.
    #[must_use]
    pub fn parts(&self) -> DecimalParts {
        DecimalParts {
            sign: if self.coefficient.is_negative() {
                DecimalSign::Negative
            } else {
                DecimalSign::Positive
            },
            magnitude: if self.coefficient.is_zero() {
                Vec::new()
            } else {
                self.coefficient.magnitude().to_bytes_be()
            },
            scale: self.scale,
        }
    }

    /// Restores a decimal from canonical persistence components.
    pub fn from_parts(parts: DecimalParts) -> Result<Self, DecimalError> {
        if parts.magnitude.first() == Some(&0) {
            return Err(DecimalError::new(
                DecimalErrorKind::Invalid,
                "decimal magnitude must use minimal-width big-endian encoding",
            ));
        }
        if parts.magnitude.is_empty() && parts.sign == DecimalSign::Negative {
            return Err(DecimalError::new(
                DecimalErrorKind::Invalid,
                "zero decimal must not use a negative sign",
            ));
        }

        let magnitude = BigUint::from_bytes_be(&parts.magnitude);
        let sign = match parts.sign {
            DecimalSign::Positive => Sign::Plus,
            DecimalSign::Negative => Sign::Minus,
        };
        Ok(Self::new(
            BigInt::from_biguint(sign, magnitude),
            parts.scale,
        ))
    }

    /// Adds two exact decimal values after aligning their base-10 scales.
    /// todo would this be better add?
    pub fn checked_add(&self, other: &Self) -> Result<Self, DecimalError> {
        let scale = self.scale.max(other.scale);
        let left = self.scaled_coefficient(scale)?;
        let right = other.scaled_coefficient(scale)?;
        Ok(Self::new(left + right, scale))
    }

    /// Subtracts two exact decimal values after aligning their base-10 scales.
    /// todo just sub?
    pub fn checked_sub(&self, other: &Self) -> Result<Self, DecimalError> {
        let scale = self.scale.max(other.scale);
        let left = self.scaled_coefficient(scale)?;
        let right = other.scaled_coefficient(scale)?;
        Ok(Self::new(left - right, scale))
    }

    /// Validates the invariant shared by monetary amounts: they may be zero,
    /// but never negative.
    pub fn validate_amount(&self) -> Result<(), DecimalError> {
        if self.coefficient.is_negative() {
            return Err(DecimalError::new(
                DecimalErrorKind::NegativeAmount,
                "currency amount must not be negative",
            ));
        }
        Ok(())
    }

    pub fn to_atomic(&self, decimals: u32) -> Result<BigUint, DecimalError> {
        self.validate_amount()?;

        let coefficient = self.coefficient.magnitude();
        let units = if self.scale <= decimals {
            coefficient * BigUint::from(10_u8).pow(decimals - self.scale)
        } else {
            let divisor = BigUint::from(10_u8).pow(self.scale - decimals);
            if coefficient % &divisor != BigUint::ZERO {
                return Err(DecimalError::new(
                    DecimalErrorKind::ExcessPrecision,
                    format!("amount has more than {decimals} fractional digits"),
                ));
            }
            coefficient / divisor
        };
        Ok(units)
    }

    pub fn to_atomic_u64(&self, decimals: u32) -> Result<u64, DecimalError> {
        let bytes = self.to_atomic(decimals)?.to_bytes_be();
        if bytes.len() > 8 {
            return Err(DecimalError::new(
                DecimalErrorKind::Overflow,
                "atomic amount exceeds the u64 range",
            ));
        }
        let mut value = [0_u8; 8];
        value[8 - bytes.len()..].copy_from_slice(&bytes);
        Ok(u64::from_be_bytes(value))
    }

    pub fn to_atomic_be_bytes<const N: usize>(
        &self,
        decimals: u32,
    ) -> Result<[u8; N], DecimalError> {
        let bytes = self.to_atomic(decimals)?.to_bytes_be();
        if bytes.len() > N {
            return Err(DecimalError::new(
                DecimalErrorKind::Overflow,
                format!("atomic amount exceeds {N} bytes"),
            ));
        }
        let mut value = [0_u8; N];
        value[N - bytes.len()..].copy_from_slice(&bytes);
        Ok(value)
    }

    fn scaled_coefficient(&self, scale: u32) -> Result<BigInt, DecimalError> {
        let exponent = scale.checked_sub(self.scale).ok_or_else(|| {
            DecimalError::new(
                DecimalErrorKind::Invalid,
                "target scale must not be smaller than the decimal scale",
            )
        })?;
        Ok(&self.coefficient * BigInt::from(BigUint::from(10_u8).pow(exponent)))
    }

    fn compare_magnitude(&self, other: &Self) -> Ordering {
        if self.coefficient.is_zero() || other.coefficient.is_zero() {
            return self
                .coefficient
                .magnitude()
                .cmp(other.coefficient.magnitude());
        }

        let left_digits = self.coefficient.magnitude().to_str_radix(10).len();
        let right_digits = other.coefficient.magnitude().to_str_radix(10).len();
        let left_exponent = left_digits as i128 - i128::from(self.scale);
        let right_exponent = right_digits as i128 - i128::from(other.scale);
        match left_exponent.cmp(&right_exponent) {
            Ordering::Equal => {}
            ordering => return ordering,
        }

        match self.scale.cmp(&other.scale) {
            Ordering::Equal => self
                .coefficient
                .magnitude()
                .cmp(other.coefficient.magnitude()),
            Ordering::Less => (self.coefficient.magnitude()
                * BigUint::from(10_u8).pow(other.scale - self.scale))
            .cmp(other.coefficient.magnitude()),
            Ordering::Greater => self.coefficient.magnitude().cmp(
                &(other.coefficient.magnitude()
                    * BigUint::from(10_u8).pow(self.scale - other.scale)),
            ),
        }
    }

    fn normalize(mut coefficient: BigInt, mut scale: u32) -> Self {
        if coefficient.is_zero() {
            return Self {
                coefficient,
                scale: 0,
            };
        }
        let ten = BigInt::from(10_u8);
        while scale > 0 && (&coefficient % &ten).is_zero() {
            coefficient /= &ten;
            scale -= 1;
        }
        Self { coefficient, scale }
    }
}

impl From<u64> for Decimal {
    fn from(value: u64) -> Self {
        Self::new(BigInt::from(value), 0)
    }
}

impl FromStr for Decimal {
    type Err = DecimalError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.trim() != value {
            return Err(DecimalError::invalid());
        }
        let (negative, unsigned) = match value.as_bytes().first() {
            Some(b'-') => (true, &value[1..]),
            Some(b'+') => (false, &value[1..]),
            _ => (false, value),
        };
        let mut parts = unsigned.split('.');
        let whole = parts.next().unwrap_or_default();
        let fraction = parts.next();
        let invalid_notation = parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|digits| {
                digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
            });
        if invalid_notation {
            return Err(DecimalError::invalid());
        }
        let fraction = fraction.unwrap_or_default();
        let scale = u32::try_from(fraction.len()).map_err(|_| DecimalError::invalid())?;
        let digits = format!("{whole}{fraction}");
        let magnitude =
            BigUint::parse_bytes(digits.as_bytes(), 10).ok_or_else(DecimalError::invalid)?;
        let sign = if negative && !magnitude.is_zero() {
            Sign::Minus
        } else {
            Sign::Plus
        };
        Ok(Self::new(BigInt::from_biguint(sign, magnitude), scale))
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.coefficient.is_negative();
        let mut digits = self.coefficient.magnitude().to_str_radix(10);
        if self.scale > 0 {
            let scale = usize::try_from(self.scale).map_err(|_| fmt::Error)?;
            if digits.len() <= scale {
                digits.insert_str(0, &"0".repeat(scale + 1 - digits.len()));
            }
            digits.insert(digits.len() - scale, '.');
        }
        if negative {
            formatter.write_str("-")?;
        }
        formatter.write_str(&digits)
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use num_bigint::BigInt;

    use super::{Decimal, DecimalErrorKind, DecimalParts, DecimalSign};

    #[test]
    fn parsing_normalizes_accepted_signs_leading_zeroes_and_fractional_scale() {
        for (input, coefficient, scale, display) in [
            ("+001.2300", 123, 2, "1.23"),
            ("-001.2300", -123, 2, "-1.23"),
            ("-0.000", 0, 0, "0"),
            ("+0", 0, 0, "0"),
            ("00012", 12, 0, "12"),
            ("0.00100", 1, 3, "0.001"),
        ] {
            let decimal = input.parse::<Decimal>().expect("valid decimal notation");
            assert_eq!(decimal.coefficient(), &BigInt::from(coefficient));
            assert_eq!(decimal.scale(), scale);
            assert_eq!(decimal.to_string(), display);
        }
    }

    #[test]
    fn parsing_rejects_invalid_signs_separators_and_non_ascii_digits() {
        for input in [
            "",
            " ",
            " 1",
            "1 ",
            "\t1",
            "1\n",
            "+",
            "-",
            ".1",
            "-.1",
            "+.1",
            "1.",
            "1..0",
            "1.2.3",
            "++1",
            "--1",
            "+-1",
            "-+1",
            "1e2",
            "1_000",
            "1,0",
            "1.-2",
            "1.+2",
            "1. 2",
            "\u{0661}",
            "1.\u{0661}",
            "\u{ff11}",
        ] {
            let error = input
                .parse::<Decimal>()
                .expect_err("invalid decimal notation");
            assert_eq!(error.kind, DecimalErrorKind::Invalid, "input: {input:?}");
            assert_eq!(
                error.message, "decimal must use canonical base-10 notation",
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn converts_human_units_to_exact_atomic_units() {
        assert_eq!(
            "1".parse::<Decimal>().unwrap().to_atomic_u64(8).unwrap(),
            100_000_000
        );
        assert_eq!(
            "1".parse::<Decimal>()
                .unwrap()
                .to_atomic(18)
                .unwrap()
                .to_string(),
            "1000000000000000000"
        );
        assert_eq!(
            "0.00000001"
                .parse::<Decimal>()
                .unwrap()
                .to_atomic_u64(8)
                .unwrap(),
            1
        );
    }

    #[test]
    fn preserves_unbounded_magnitude_and_exact_display() {
        let value = "123456789012345678901234567890.000000000000000001"
            .parse::<Decimal>()
            .unwrap();
        assert_eq!(
            value.to_string(),
            "123456789012345678901234567890.000000000000000001"
        );
    }

    #[test]
    fn rejects_negative_currency_and_excess_precision() {
        assert_eq!(
            "-1".parse::<Decimal>()
                .unwrap()
                .to_atomic(8)
                .unwrap_err()
                .kind,
            DecimalErrorKind::NegativeAmount
        );
        assert_eq!(
            "0.000000001"
                .parse::<Decimal>()
                .unwrap()
                .to_atomic(8)
                .unwrap_err()
                .kind,
            DecimalErrorKind::ExcessPrecision
        );
    }

    #[test]
    fn zero_is_canonical_and_nonnegative() {
        let zero = Decimal::zero();
        assert!(zero.is_zero());
        assert_eq!(zero.scale(), 0);
        assert_eq!(zero.to_string(), "0");
        assert_eq!(zero.validate_amount(), Ok(()));
    }

    #[test]
    fn checked_arithmetic_aligns_scales_and_normalizes_results() {
        let left = "12345678901234567890.125".parse::<Decimal>().unwrap();
        let right = "0.875".parse::<Decimal>().unwrap();
        assert_eq!(
            left.checked_add(&right).unwrap().to_string(),
            "12345678901234567891"
        );
        assert_eq!(
            right.checked_sub(&left).unwrap().to_string(),
            "-12345678901234567889.25"
        );
    }

    #[test]
    fn monetary_validation_rejects_negative_arithmetic_results() {
        let result = Decimal::from(1).checked_sub(&Decimal::from(2)).unwrap();
        assert_eq!(
            result.validate_amount().unwrap_err().kind,
            DecimalErrorKind::NegativeAmount
        );
    }

    #[test]
    fn persistence_parts_round_trip_without_precision_loss() {
        for value in [
            "0",
            "-0.000000000000000001",
            "123456789012345678901234567890.000000000000000001",
        ] {
            let decimal = value.parse::<Decimal>().unwrap();
            assert_eq!(Decimal::from_parts(decimal.parts()).unwrap(), decimal);
        }
    }

    #[test]
    fn persistence_parts_reject_noncanonical_sign_and_magnitude() {
        let negative_zero = DecimalParts {
            sign: DecimalSign::Negative,
            magnitude: Vec::new(),
            scale: 0,
        };
        assert_eq!(
            Decimal::from_parts(negative_zero).unwrap_err().kind,
            DecimalErrorKind::Invalid
        );

        let leading_zero = DecimalParts {
            sign: DecimalSign::Positive,
            magnitude: vec![0, 1],
            scale: 0,
        };
        assert_eq!(
            Decimal::from_parts(leading_zero).unwrap_err().kind,
            DecimalErrorKind::Invalid
        );
    }

    #[test]
    fn ordering_compares_numeric_values_instead_of_structural_parts() {
        let smaller = "0.00059859".parse::<Decimal>().unwrap();
        let larger = "0.0006".parse::<Decimal>().unwrap();

        assert!(smaller < larger);
        assert!(larger > smaller);
    }

    #[test]
    fn ordering_handles_unbounded_magnitudes_and_distant_scales() {
        let huge = "99999999999999999999999999999999999999999999999999"
            .parse::<Decimal>()
            .unwrap();
        let tiny = Decimal::new(BigInt::from(1_u8), 1_000_000);

        assert!(huge > tiny);
        assert!(tiny > Decimal::zero());
    }

    #[test]
    fn ordering_agrees_with_canonical_equality() {
        let parsed = "1.2300".parse::<Decimal>().unwrap();
        let constructed = Decimal::new(BigInt::from(123_u8), 2);

        assert_eq!(parsed, constructed);
        assert_eq!(parsed.cmp(&constructed), Ordering::Equal);
    }

    #[test]
    fn ordering_handles_negative_values_across_scales() {
        let farther_from_zero = "-1000000000000000000000000000000.1"
            .parse::<Decimal>()
            .unwrap();
        let closer_to_zero = "-0.00000000000000000001".parse::<Decimal>().unwrap();

        assert!(farther_from_zero < closer_to_zero);
        assert!(closer_to_zero < Decimal::zero());
    }

    #[test]
    fn ordering_is_antisymmetric_across_zero_and_maximum_scale() {
        let values = [
            Decimal::new(BigInt::from(-1), 0),
            "-0.0006".parse::<Decimal>().unwrap(),
            "-0.00059859".parse::<Decimal>().unwrap(),
            Decimal::new(BigInt::from(-1), u32::MAX),
            Decimal::new(BigInt::from(0), u32::MAX),
            Decimal::new(BigInt::from(1), u32::MAX),
            "0.00059859".parse::<Decimal>().unwrap(),
            "0.0006".parse::<Decimal>().unwrap(),
            Decimal::from(1),
        ];
        assert_eq!(values[4].scale(), 0);
        for (left_index, left) in values.iter().enumerate() {
            for (right_index, right) in values.iter().enumerate() {
                assert_eq!(left.cmp(right), left_index.cmp(&right_index));
                assert_eq!(left.partial_cmp(right), Some(left_index.cmp(&right_index)));
            }
        }
    }

    #[test]
    fn arithmetic_keeps_exact_signed_values_across_different_scales() {
        let left = "-12345678901234567890.1".parse::<Decimal>().unwrap();
        let right = "0.000000000000000001".parse::<Decimal>().unwrap();
        assert_eq!(
            left.checked_add(&right).unwrap().to_string(),
            "-12345678901234567890.099999999999999999"
        );
        assert_eq!(
            left.checked_sub(&right).unwrap().to_string(),
            "-12345678901234567890.100000000000000001"
        );
        assert_eq!(right.checked_add(&left), left.checked_add(&right));
        assert_eq!(left.checked_sub(&left).unwrap(), Decimal::zero());
    }

    #[test]
    fn coefficient_scaling_rejects_reduction_and_preserves_the_decimal() {
        let value = "-1.25".parse::<Decimal>().unwrap();
        let original = value.clone();
        assert_eq!(value.scaled_coefficient(2).unwrap(), BigInt::from(-125));
        assert_eq!(value.scaled_coefficient(4).unwrap(), BigInt::from(-12500));
        let error = value.scaled_coefficient(1).unwrap_err();
        assert_eq!(error.kind, DecimalErrorKind::Invalid);
        assert_eq!(
            error.message,
            "target scale must not be smaller than the decimal scale"
        );
        assert_eq!(value, original);
        assert_eq!(
            Decimal::new(BigInt::from(1), u32::MAX)
                .scaled_coefficient(u32::MAX)
                .unwrap(),
            BigInt::from(1)
        );
    }
}
