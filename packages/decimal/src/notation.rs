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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_normalizes_accepted_signs_leading_zeroes_and_fractional_scale() {
        for (input, scale, display) in [
            ("+001.2300", 2, "1.23"),
            ("-001.2300", 2, "-1.23"),
            ("-0.000", 0, "0"),
            ("+0", 0, "0"),
            ("00012", 0, "12"),
            ("0.00100", 3, "0.001"),
        ] {
            let decimal = input.parse::<Decimal>().expect("valid decimal notation");
            assert_eq!(decimal.scale(), scale, "input: {input:?}");
            assert_eq!(decimal.to_string(), display, "input: {input:?}");
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
            assert_eq!(
                error.message, "decimal must use canonical base-10 notation",
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn parsing_rejects_text_longer_than_the_accepted_bound() {
        let longest = format!("0.{}", "1".repeat(Decimal::MAX_LEN - 2));
        assert_eq!(longest.len(), Decimal::MAX_LEN);
        assert_eq!(
            longest.parse::<Decimal>().expect("bounded text").scale(),
            u32::try_from(Decimal::MAX_LEN - 2).expect("bounded scale")
        );

        let oversized = format!("0.{}", "1".repeat(Decimal::MAX_LEN - 1));
        assert_eq!(
            oversized
                .parse::<Decimal>()
                .expect_err("oversized text")
                .message,
            "decimal must use canonical base-10 notation"
        );
    }

    #[test]
    fn formatting_stays_positional_for_the_smallest_chain_units() {
        for (atomic, decimals, text) in [
            (1_u64, 8, "0.00000001"),
            (1, 9, "0.000000001"),
            (1, 18, "0.000000000000000001"),
            (1_500_000, 8, "0.015"),
        ] {
            assert_eq!(
                Decimal::from_atomic(BigUint::from(atomic), decimals).to_string(),
                text
            );
        }
    }

    /// Every shape the plain writer must produce, pinned as text.
    ///
    /// `Display` delegates to `BigDecimal::write_plain_string`, so this is the
    /// contract that keeps an upstream change from moving persisted rows and
    /// HTTP bodies: the four digit shapes (no fraction, fraction shorter than
    /// the digits, fraction exactly as long, fraction longer — which needs the
    /// leading `0.`), both signs, and zero. The width, zero-fill and `+` specs
    /// are ignored, which is what the writer did before it was delegated; a
    /// version that started honouring them would fail here rather than in a
    /// stored amount.
    #[test]
    fn display_is_plain_positional_at_every_shape() {
        for (text, expected) in [
            ("0", "0"),
            ("-0.000", "0"),
            ("1000", "1000"),
            ("-1000", "-1000"),
            ("1.23", "1.23"),
            ("-1.23", "-1.23"),
            ("9.9", "9.9"),
            ("0.5", "0.5"),
            ("-0.5", "-0.5"),
            ("0.015", "0.015"),
            ("0.00000001", "0.00000001"),
            ("-0.000000000000000001", "-0.000000000000000001"),
            (
                "123456789012345678901234567890.000000000000000001",
                "123456789012345678901234567890.000000000000000001",
            ),
        ] {
            let value = text.parse::<Decimal>().expect("canonical decimal notation");
            assert_eq!(value.to_string(), expected, "input: {text:?}");
            assert_eq!(format!("{value:>24}"), expected, "width is ignored");
            assert_eq!(format!("{value:024}"), expected, "zero fill is ignored");
            assert_eq!(
                format!("{value:+}"),
                expected,
                "an explicit sign is ignored"
            );
        }
    }

    #[test]
    fn debug_shows_the_canonical_text() {
        let value = "1.23".parse::<Decimal>().expect("canonical decimal");
        assert_eq!(format!("{value:?}"), "Decimal(1.23)");
    }
}
