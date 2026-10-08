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
