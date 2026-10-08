use std::{fmt, str::FromStr};

use base::Decimal;
use serde_json::{Number, Value};

use super::{BTC, Error};

/// Satoshis in one bitcoin.
const SATOSHIS_PER_BITCOIN: u64 = 100_000_000;

/// A Bitcoin amount in satoshis, the only unit this crate computes with.
///
/// Bitcoin Core writes amounts as fixed-point BTC JSON numbers;
/// [`Satoshi::from_btc`] and [`Satoshi::btc`] are the only conversions
/// between the two, and they never go through floating point.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Satoshi(u64);

impl Satoshi {
    /// No satoshis.
    pub const ZERO: Self = Self(0);

    /// `satoshis` satoshis.
    #[must_use]
    pub const fn from_atomic(satoshis: u64) -> Self {
        Self(satoshis)
    }

    /// This amount in the chain's own atomic width, the exact inverse of
    /// [`Satoshi::from_atomic`].
    #[must_use]
    pub const fn units(self) -> u64 {
        self.0
    }

    /// `self + other`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `self - other`, or `None` below zero.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(difference) => Some(Self(difference)),
            None => None,
        }
    }

    /// Converts a positive BTC amount (8 decimals) to satoshis, without
    /// rounding.
    ///
    /// # Errors
    ///
    /// [`super::ErrorKind::InvalidTransaction`] for zero, a negative amount,
    /// more than 8 decimals, or more than `u64` satoshis.
    pub fn from_decimal(value: &Decimal) -> Result<Self, Error> {
        let amount = value
            .to_atomic_u64(BTC.decimals)
            .map(Self)
            .map_err(|error| Error::invalid_transaction(error.message))?;
        if amount.0 == 0 {
            return Err(Error::invalid_transaction(
                "Bitcoin amount must be positive",
            ));
        }
        Ok(amount)
    }

    /// This amount in BTC (8 decimals).
    #[must_use]
    pub fn decimal(self) -> Decimal {
        Decimal::from_atomic(self.0.into(), BTC.decimals)
    }

    /// This amount as a count of satoshis (0 decimals), as indexing stores it.
    #[must_use]
    pub fn atomic(self) -> Decimal {
        Decimal::from(self.0)
    }

    /// Reads an exact fixed-point BTC JSON number such as `0.00001001`.
    ///
    /// # Errors
    ///
    /// A message prefixed with `context` for a non-number, a negative or
    /// exponent form, more than 8 decimals, or more than `u64` satoshis.
    pub fn from_btc(value: &Value, context: &str) -> Result<Self, String> {
        let lexical = value
            .as_number()
            .map(Number::to_string)
            .ok_or_else(|| format!("{context} must be a JSON number"))?;
        if lexical.starts_with('-') || lexical.contains(['e', 'E', '+']) {
            return Err(format!(
                "{context} must be a non-negative fixed-point decimal"
            ));
        }
        let (whole, fraction) = lexical.split_once('.').unwrap_or((&lexical, ""));
        let digits = |text: &str| text.bytes().all(|byte| byte.is_ascii_digit());
        if whole.is_empty() || !digits(whole) || !digits(fraction) || fraction.len() > 8 {
            return Err(format!("{context} is not an exact Bitcoin amount"));
        }
        let overflow = || format!("{context} exceeds u64 satoshis");
        let whole = whole.parse::<u64>().map_err(|_| overflow())?;
        let fraction = format!("{fraction:0<8}")
            .parse::<u64>()
            .map_err(|_| format!("{context} is invalid"))?;
        whole
            .checked_mul(SATOSHIS_PER_BITCOIN)
            .and_then(|satoshis| satoshis.checked_add(fraction))
            .map(Self)
            .ok_or_else(overflow)
    }

    /// This amount as the shortest exact BTC JSON number, e.g. `0.00001001`.
    #[must_use]
    pub fn btc(self) -> Number {
        Number::from_str(&self.to_string()).expect("a BTC decimal is a valid JSON number")
    }
}

/// The shortest exact BTC text: `1`, `0.00001`, `0.00001001`.
impl fmt::Display for Satoshi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / SATOSHIS_PER_BITCOIN;
        let remainder = self.0 % SATOSHIS_PER_BITCOIN;
        if remainder == 0 {
            return write!(formatter, "{whole}");
        }
        let fraction = format!("{remainder:08}");
        write!(formatter, "{whole}.{}", fraction.trim_end_matches('0'))
    }
}
