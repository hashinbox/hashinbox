//! Canonical base-10 text: the only spelling this crate reads or writes.
//!
//! Parsing is owned here rather than delegated to the underlying big-decimal
//! implementation, whose own `FromStr` accepts exponents. Formatting is
//! delegated to `BigDecimal::write_plain_string`, which is a different writer
//! from that type's `Display`: `Display` switches to exponential form for small
//! magnitudes, while the plain writer never does. Persisted rows and HTTP
//! bodies carry the text produced here, and `display_is_plain_positional_at_
//! every_shape` below pins every shape it must keep producing, so an upstream
//! change cannot move it silently.

use std::{fmt, str::FromStr};

use bigdecimal::{BigDecimal, Zero};
use num_bigint::{BigInt, BigUint, Sign};

use crate::{Decimal, Error};

/// The signed digit sections of one canonical base-10 literal.
struct Digits<'a> {
    negative: bool,
    whole: &'a str,
    fraction: &'a str,
}

impl<'a> Digits<'a> {
    /// Splits `text` into its sign and digit runs.
    ///
    /// Accepts an optional leading `+`/`-`, at least one ASCII whole digit,
    /// and an optional `.` followed by at least one ASCII fractional digit.
    /// Everything else — surrounding space, exponents, digit separators,
    /// non-ASCII digits, a bare or trailing point, or text longer than
    /// [`Decimal::MAX_LEN`] — is rejected.
    fn parse(text: &'a str) -> Result<Self, Error> {
        if text.is_empty() || text.len() > Decimal::MAX_LEN || text.trim() != text {
            return Err(Error::invalid());
        }
        let (negative, unsigned) = match text.as_bytes().first() {
            Some(b'-') => (true, &text[1..]),
            Some(b'+') => (false, &text[1..]),
            _ => (false, text),
        };

        let mut sections = unsigned.split('.');
        let whole = sections.next().unwrap_or_default();
        let fraction = sections.next();
        let malformed = sections.next().is_some()
            || whole.is_empty()
            || !Self::is_digits(whole)
            || fraction.is_some_and(|digits| digits.is_empty() || !Self::is_digits(digits));
        if malformed {
            return Err(Error::invalid());
        }

        Ok(Self {
            negative,
            whole,
            fraction: fraction.unwrap_or_default(),
        })
    }

    fn is_digits(text: &str) -> bool {
        text.bytes().all(|byte| byte.is_ascii_digit())
    }

    /// The exact value these digits spell.
    fn decimal(&self) -> Result<Decimal, Error> {
        let scale = i64::try_from(self.fraction.len()).map_err(|_| Error::invalid())?;
        let digits = format!("{}{}", self.whole, self.fraction);
        let magnitude = BigUint::parse_bytes(digits.as_bytes(), 10).ok_or_else(Error::invalid)?;
        let sign = if self.negative && !magnitude.is_zero() {
            Sign::Minus
        } else {
            Sign::Plus
        };
        Ok(Decimal::canonical(BigDecimal::new(
            BigInt::from_biguint(sign, magnitude),
            scale,
        )))
    }
}

impl FromStr for Decimal {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Digits::parse(text)?.decimal()
    }
}

impl fmt::Display for Decimal {
    /// Writes plain positional text, never an exponent: one satoshi is
    /// `0.00000001` and one lamport is `0.000000001`. Trailing fractional
    /// zeros were dropped when the value was built, so the output parses back
    /// to the same value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.write_plain_string(formatter)
    }
}

impl fmt::Debug for Decimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Decimal({self})")
    }
}
