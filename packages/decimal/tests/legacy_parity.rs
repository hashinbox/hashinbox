//! Differential test: the wrapped `bigdecimal` core must agree, character for
//! character, with the hand-rolled `BigInt + scale` implementation it
//! replaced.
//!
//! [`Legacy`] below is that implementation, copied verbatim from the code this
//! crate supersedes (`sdk/base/src/decimal.rs`) and reduced to the surface
//! that survived. Persisted rows and HTTP bodies carry `Display` output and
//! both repositories re-parse and re-compare it, so any divergence in text,
//! acceptance, ordering, or conversion would silently break stored data.
//!
//! The one deliberate divergence is acceptance: the new parser rejects text
//! longer than `Decimal::MAX_LEN`, which the legacy parser accepted. The
//! corpus stays inside that bound, and [`bounds_are_the_only_divergence`]
//! pins the difference.

use std::{cmp::Ordering, str::FromStr};

use decimal::Decimal;
use num_bigint::{BigInt, BigUint, Sign};

// ---------------------------------------------------------------------------
// The superseded implementation, kept only as the reference for this test.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct Legacy {
    coefficient: BigInt,
    scale: u32,
}

impl Ord for Legacy {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.coefficient.sign(), other.coefficient.sign()) {
            (Sign::Minus, Sign::Minus) => other.compare_magnitude(self),
            (Sign::Minus, _) => Ordering::Less,
            (_, Sign::Minus) => Ordering::Greater,
            _ => self.compare_magnitude(other),
        }
    }
}

impl PartialOrd for Legacy {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Legacy {
    fn new(coefficient: BigInt, scale: u32) -> Self {
        Self::normalize(coefficient, scale)
    }

    fn from_atomic(units: BigUint, decimals: u32) -> Self {
        Self::new(BigInt::from(units), decimals)
    }

    fn scale(&self) -> u32 {
        self.scale
    }

    fn is_zero(&self) -> bool {
        self.coefficient == BigInt::ZERO
    }

    fn validate_amount(&self) -> Result<(), String> {
        if self.coefficient.sign() == Sign::Minus {
            return Err("currency amount must not be negative".to_owned());
        }
        Ok(())
    }

    fn to_atomic(&self, decimals: u32) -> Result<BigUint, String> {
        self.validate_amount()?;

        let coefficient = self.coefficient.magnitude();
        let units = if self.scale <= decimals {
            coefficient * BigUint::from(10_u8).pow(decimals - self.scale)
        } else {
            let divisor = BigUint::from(10_u8).pow(self.scale - decimals);
            if coefficient % &divisor != BigUint::ZERO {
                return Err(format!("amount has more than {decimals} fractional digits"));
            }
            coefficient / divisor
        };
        Ok(units)
    }

    fn to_atomic_u64(&self, decimals: u32) -> Result<u64, String> {
        let bytes = self.to_atomic(decimals)?.to_bytes_be();
        if bytes.len() > 8 {
            return Err("atomic amount exceeds the u64 range".to_owned());
        }
        let mut value = [0_u8; 8];
        value[8 - bytes.len()..].copy_from_slice(&bytes);
        Ok(u64::from_be_bytes(value))
    }

    fn to_atomic_be_bytes<const N: usize>(&self, decimals: u32) -> Result<[u8; N], String> {
        let bytes = self.to_atomic(decimals)?.to_bytes_be();
        if bytes.len() > N {
            return Err(format!("atomic amount exceeds {N} bytes"));
        }
        let mut value = [0_u8; N];
        value[N - bytes.len()..].copy_from_slice(&bytes);
        Ok(value)
    }

    fn compare_magnitude(&self, other: &Self) -> Ordering {
        if self.is_zero() || other.is_zero() {
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
        if coefficient == BigInt::ZERO {
            return Self {
                coefficient,
                scale: 0,
            };
        }
        let ten = BigInt::from(10_u8);
        while scale > 0 && (&coefficient % &ten) == BigInt::ZERO {
            coefficient /= &ten;
            scale -= 1;
        }
        Self { coefficient, scale }
    }
}

impl From<u64> for Legacy {
    fn from(value: u64) -> Self {
        Self::new(BigInt::from(value), 0)
    }
}

impl FromStr for Legacy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || "decimal must use canonical base-10 notation".to_owned();
        if value.is_empty() || value.trim() != value {
            return Err(invalid());
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
            return Err(invalid());
        }
        let fraction = fraction.unwrap_or_default();
        let scale = u32::try_from(fraction.len()).map_err(|_| invalid())?;
        let digits = format!("{whole}{fraction}");
        let magnitude = BigUint::parse_bytes(digits.as_bytes(), 10).ok_or_else(invalid)?;
        let sign = if negative && magnitude != BigUint::ZERO {
            Sign::Minus
        } else {
            Sign::Plus
        };
        Ok(Self::new(BigInt::from_biguint(sign, magnitude), scale))
    }
}

impl std::fmt::Display for Legacy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let negative = self.coefficient.sign() == Sign::Minus;
        let mut digits = self.coefficient.magnitude().to_str_radix(10);
        if self.scale > 0 {
            let scale = usize::try_from(self.scale).map_err(|_| std::fmt::Error)?;
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

// ---------------------------------------------------------------------------
// The corpus.
// ---------------------------------------------------------------------------

/// A reproducible xorshift source, so the wide corpus is the same on every
/// machine and every run without adding a property-testing dependency.
struct Source(u64);

impl Source {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).unwrap_or_default()
    }

    /// One arbitrary literal: optional sign, 0-40 whole digits, optional point
    /// and 0-40 fractional digits, with leading and trailing zeros common.
    fn literal(&mut self) -> String {
        let mut text = String::new();
        match self.below(6) {
            0 => text.push('-'),
            1 => text.push('+'),
            _ => {}
        }
        let whole = self.below(41);
        let fraction = self.below(41);
        self.digits(&mut text, whole.max(1));
        if fraction > 0 {
            text.push('.');
            self.digits(&mut text, fraction);
        }
        text
    }

    fn digits(&mut self, text: &mut String, count: usize) {
        for _ in 0..count {
            let digit = match self.below(4) {
                0 => 0,
                1 => 9,
                _ => self.below(10),
            };
            text.push(char::from(b'0' + u8::try_from(digit).unwrap_or_default()));
        }
    }
}

/// Every literal the superseded implementation's own tests used, every value
/// the storage and chain crates spell out, and the boundaries of each
/// supported chain.
fn fixed_corpus() -> Vec<String> {
    let max_wei = BigUint::from_bytes_be(&[0xff; 32]);
    let mut corpus: Vec<String> = [
        // acceptance, from the superseded parser's own tests
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
        "1E2",
        "1_000",
        "1,0",
        "1.-2",
        "1.+2",
        "1. 2",
        "\u{0661}",
        "1.\u{0661}",
        "\u{ff11}",
        "0x10",
        "NaN",
        "inf",
        "1e-8",
        "1.0e2",
        // canonical and non-canonical spellings of the same values
        "+001.2300",
        "-001.2300",
        "-0.000",
        "+0",
        "00012",
        "0.00100",
        "0",
        "-0",
        "0.0",
        "1.0",
        "01",
        "1.2300",
        "0.015",
        "1.23000000",
        "1.22000000",
        "0.01000000",
        "2.25",
        // the smallest unit of each chain
        "0.00000001",
        "0.000000001",
        "0.000000000000000001",
        "0.0000000001",
        // values the chain and storage crates use verbatim
        "0.00001",
        "0.000005",
        "0.1",
        "1",
        "5",
        "7",
        "12",
        "100",
        "1000",
        "1250000",
        "1500000",
        "21000000000000",
        "18446744073.709551615",
        "1000000000000000000",
        "12345678901234567890.125",
        "0.875",
        "-12345678901234567890.1",
        "12345678901234567890.1",
        "0.000000000000000001",
        "-0.000000000000000001",
        "0.00059859",
        "0.0006",
        "-0.0006",
        "-0.00059859",
        "1000000000000000000000000000000.1",
        "-1000000000000000000000000000000.1",
        "0.00000000000000000001",
        "-0.00000000000000000001",
        // 30+ digit magnitudes
        "123456789012345678901234567890.000000000000000001",
        "1234567890123456789012345678901234567890.000000000000000001",
        "99999999999999999999999999999999999999999999999999",
        "-99999999999999999999999999999999999999999999999999",
    ]
    .iter()
    .map(|text| (*text).to_owned())
    .collect();

    // 2^256 - 1 atomic units at scale 0 and scale 18, and its neighbours.
    for decimals in [0_u32, 8, 9, 18] {
        for offset in [0_u64, 1] {
            let units = max_wei.clone() - BigUint::from(offset);
            corpus.push(Legacy::from_atomic(units, decimals).to_string());
        }
    }
    // u64 boundaries at every chain's decimals.
    for decimals in [0_u32, 8, 9, 18] {
        for units in [0_u64, 1, 9, 10, u64::MAX, u64::MAX - 1] {
            corpus.push(Legacy::from_atomic(BigUint::from(units), decimals).to_string());
        }
    }
    // Trailing-zero and leading-zero families.
    for zeros in [1_usize, 2, 7, 17, 18, 19, 40] {
        corpus.push(format!("1{}", "0".repeat(zeros)));
        corpus.push(format!("1.{}", "0".repeat(zeros)));
        corpus.push(format!("0.{}1", "0".repeat(zeros)));
        corpus.push(format!("{}1", "0".repeat(zeros)));
        corpus.push(format!("-0.{}1", "0".repeat(zeros)));
    }
    // The widest text the new parser accepts, and one character past it.
    corpus.push(format!("0.{}", "1".repeat(Decimal::MAX_LEN - 2)));
    corpus.push("9".repeat(Decimal::MAX_LEN));
    corpus
}

fn corpus() -> Vec<String> {
    let mut corpus = fixed_corpus();
    let mut source = Source(0x2545_f491_4f6c_dd1d);
    for _ in 0..20_000 {
        corpus.push(source.literal());
    }
    corpus
}

// ---------------------------------------------------------------------------
// The comparisons.
// ---------------------------------------------------------------------------

#[test]
fn parsing_display_and_round_trips_are_identical() {
    for text in corpus() {
        let expected = Legacy::from_str(&text);
        let observed = Decimal::from_str(&text);
        match (expected, observed) {
            (Err(expected), Err(observed)) => {
                assert_eq!(observed.message, expected, "input: {text:?}");
            }
            (Ok(expected), Ok(observed)) => {
                assert_eq!(
                    observed.to_string(),
                    expected.to_string(),
                    "input: {text:?}"
                );
                assert_eq!(observed.scale(), expected.scale(), "input: {text:?}");
                assert_eq!(observed.is_zero(), expected.is_zero(), "input: {text:?}");
                // FromStr o Display is the identity on canonical text.
                let reparsed = Decimal::from_str(&observed.to_string());
                assert_eq!(
                    reparsed.expect("canonical text"),
                    observed,
                    "input: {text:?}"
                );
            }
            (expected, observed) => {
                assert!(
                    text.len() > Decimal::MAX_LEN,
                    "acceptance differs inside the bound: {text:?} {expected:?} {observed:?}"
                );
            }
        }
    }
}

#[test]
fn ordering_is_identical_and_total() {
    let values: Vec<(Legacy, Decimal)> = corpus()
        .iter()
        .filter_map(|text| Some((Legacy::from_str(text).ok()?, Decimal::from_str(text).ok()?)))
        .collect();

    let mut source = Source(0x1234_5678_9abc_def1);
    for _ in 0..200_000 {
        let left = &values[source.below(values.len())];
        let right = &values[source.below(values.len())];
        let expected = left.0.cmp(&right.0);
        assert_eq!(left.1.cmp(&right.1), expected, "{} <=> {}", left.1, right.1);
        assert_eq!(
            right.1.cmp(&left.1),
            expected.reverse(),
            "antisymmetry: {} <=> {}",
            left.1,
            right.1
        );
        assert_eq!(left.1 == right.1, expected == Ordering::Equal);
    }
}

#[test]
fn atomic_conversions_are_identical() {
    for text in corpus() {
        let (Ok(expected), Ok(observed)) = (Legacy::from_str(&text), Decimal::from_str(&text))
        else {
            continue;
        };
        for decimals in [0_u32, 1, 6, 8, 9, 18, 36, 255] {
            assert_eq!(
                observed.to_atomic(decimals).map_err(|error| error.message),
                expected.to_atomic(decimals),
                "to_atomic({decimals}) of {text:?}"
            );
            assert_eq!(
                observed
                    .to_atomic_u64(decimals)
                    .map_err(|error| error.message),
                expected.to_atomic_u64(decimals),
                "to_atomic_u64({decimals}) of {text:?}"
            );
            assert_eq!(
                observed
                    .to_atomic_be_bytes::<32>(decimals)
                    .map_err(|error| error.message),
                expected.to_atomic_be_bytes::<32>(decimals),
                "to_atomic_be_bytes::<32>({decimals}) of {text:?}"
            );
        }
        assert_eq!(
            observed.validate_amount().map_err(|error| error.message),
            expected.validate_amount()
        );
    }
}

#[test]
fn integer_and_atomic_constructors_are_identical() {
    let mut source = Source(0xdead_beef_cafe_f00d);
    for _ in 0..5_000 {
        let units = source.next();
        let decimals = u32::try_from(source.below(40)).expect("small scale");
        assert_eq!(
            Decimal::from(units).to_string(),
            Legacy::from(units).to_string()
        );
        assert_eq!(
            Decimal::from_atomic(BigUint::from(units), decimals).to_string(),
            Legacy::from_atomic(BigUint::from(units), decimals).to_string(),
            "from_atomic({units}, {decimals})"
        );
    }
    assert_eq!(Decimal::zero().to_string(), Legacy::from(0).to_string());
}

#[test]
fn bounds_are_the_only_divergence() {
    // The legacy parser accepted unbounded text; the new one stops at
    // MAX_LEN. Nothing shorter is rejected that the legacy parser accepted.
    let oversized = format!("0.{}", "1".repeat(Decimal::MAX_LEN));
    assert!(Legacy::from_str(&oversized).is_ok());
    assert_eq!(
        Decimal::from_str(&oversized)
            .expect_err("oversized text")
            .message,
        "decimal must use canonical base-10 notation"
    );

    // The longest legitimate amount — 2^256-1 atomic units of a token that
    // declares the widest possible `decimals` (255, a uint8) — fits.
    let widest = Legacy::from_atomic(BigUint::from_bytes_be(&[0xff; 32]), 255).to_string();
    assert!(widest.len() < Decimal::MAX_LEN, "{}", widest.len());
    assert_eq!(
        Decimal::from_str(&widest)
            .expect("widest legitimate amount")
            .to_string(),
        widest
    );
}
