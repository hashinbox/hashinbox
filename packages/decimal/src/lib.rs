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
/// Arithmetic is exact: `+`, `-`, `*` (and their assigning forms, by value
/// or by reference) never round, and [`Decimal::checked_div`] refuses zero.
/// A difference may be negative; [`Decimal::validate_amount`] checks a
/// monetary amount.
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

    /// `self / divisor`, rounded to 100 significant digits when it does not
    /// terminate; `None`
    /// for a zero divisor.
    #[must_use]
    pub fn checked_div(&self, divisor: &Self) -> Option<Self> {
        if divisor.is_zero() {
            return None;
        }
        Some(Self::canonical(&self.0 / &divisor.0))
    }
}

/// `Decimal op Decimal` for every owned/borrowed pairing, and `op=`.
macro_rules! arithmetic {
    ($op:ident, $method:ident, $assign:ident, $assign_method:ident) => {
        impl std::ops::$op<&Decimal> for &Decimal {
            type Output = Decimal;

            fn $method(self, other: &Decimal) -> Decimal {
                Decimal::canonical(std::ops::$op::$method(&self.0, &other.0))
            }
        }

        impl std::ops::$op for Decimal {
            type Output = Self;

            fn $method(self, other: Self) -> Self {
                std::ops::$op::$method(&self, &other)
            }
        }

        impl std::ops::$op<&Decimal> for Decimal {
            type Output = Self;

            fn $method(self, other: &Self) -> Self {
                std::ops::$op::$method(&self, other)
            }
        }

        impl std::ops::$assign<&Decimal> for Decimal {
            fn $assign_method(&mut self, other: &Self) {
                *self = std::ops::$op::$method(&*self, other);
            }
        }

        impl std::ops::$assign for Decimal {
            fn $assign_method(&mut self, other: Self) {
                *self = std::ops::$op::$method(&*self, &other);
            }
        }
    };
}

arithmetic!(Add, add, AddAssign, add_assign);
arithmetic!(Sub, sub, SubAssign, sub_assign);
arithmetic!(Mul, mul, MulAssign, mul_assign);

impl std::iter::Sum for Decimal {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::zero(), |sum, value| sum + value)
    }
}

impl<'a> std::iter::Sum<&'a Decimal> for Decimal {
    fn sum<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
        Self::total(iter)
    }
}

impl From<u64> for Decimal {
    fn from(value: u64) -> Self {
        Self::canonical(BigDecimal::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::Decimal;

    fn value(text: &str) -> Decimal {
        text.parse().expect("decimal")
    }

    #[test]
    fn arithmetic_is_exact_and_canonical() {
        assert_eq!(value("1.10") + value("2.9"), value("4"));
        assert_eq!(value("1") - value("1.5"), value("-0.5"));
        assert_eq!(value("1.5") * value("2"), value("3"));
        assert_eq!(value("1").checked_div(&value("4")), Some(value("0.25")));
        assert_eq!(value("1").checked_div(&Decimal::zero()), None);
        let mut sum = Decimal::zero();
        sum += value("0.1");
        sum -= value("0.3");
        assert_eq!(sum, value("-0.2"));
        assert_eq!([value("1"), value("2")].iter().sum::<Decimal>(), value("3"));
    }
}
