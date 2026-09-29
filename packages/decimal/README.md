# decimal: exact base-10 amounts

## Why this crate exists

Money must survive a round trip through a chain, a database row, and an HTTP
body without changing by one atomic unit. Binary floating point cannot do
that, and a fixed 96-bit or 128-bit decimal cannot hold `2^256 - 1` wei. This
crate owns one value type that is exact at any magnitude, orders numerically
across unequal scales, and prints one canonical plain positional spelling that
parses back to itself.

The arithmetic core is [`bigdecimal`], a maintained arbitrary-precision
decimal over `num-bigint`: normalization, numeric equality, numeric ordering,
and exact rescaling are its code, not ours. The text layer is ours on purpose.
`bigdecimal`'s own `Display` switches to exponential notation below a
build-time threshold — it would print one satoshi as `1E-8` — and its own
`FromStr` accepts exponents. Persisted rows are re-parsed and re-compared
against their stored text, so that spelling is a durable contract and cannot
be delegated to an upstream default.

It deliberately does not own arithmetic, rounding, division, currency codes,
asset metadata, or Serde. Amounts cross every boundary as text, and the code
that adds or subtracts them does so in the chain's own atomic integer width.

## Concepts

| Concept | Type | Notes |
|---|---|---|
| exact amount | [`Decimal`] | `coefficient * 10^-scale`, canonical, no arithmetic |
| canonical text | [`Decimal`]'s `Display`/`FromStr` | plain positional, trailing fractional zeros dropped |
| accepted text bound | [`Decimal::MAX_LEN`] | the only untrusted entry point is parsing |
| atomic units | [`Decimal::to_atomic`] and friends | exact, never rounded |
| failure | [`Error`] | one type, one `message` |

## Public API map

| Group | Items |
|---|---|
| construct | [`Decimal::zero`], [`Decimal::from_atomic`], `Decimal::from(u64)`, `"1.5".parse::<Decimal>()` |
| inspect | [`Decimal::scale`], [`Decimal::is_zero`], [`Decimal::validate_amount`], `to_string()` |
| convert | [`Decimal::to_atomic`], [`Decimal::to_atomic_u64`], [`Decimal::to_atomic_be_bytes`] |
| compare | `Ord`, `PartialOrd`, `Eq`, `Hash` — all numeric, across unequal scales |
| errors | [`Error`] |

## Usage

```rust
use decimal::Decimal;

# fn main() -> Result<(), decimal::Error> {
// Text is canonical: trailing fractional zeros are dropped and the
// spelling round-trips.
let amount: Decimal = "0.01500".parse()?;
assert_eq!(amount.to_string(), "0.015");
assert_eq!(amount.scale(), 3);

// Display units convert to atomic units exactly, or not at all.
assert_eq!(amount.to_atomic_u64(8)?, 1_500_000);
assert!(amount.to_atomic(2).is_err()); // would have to round

// Small units never print as exponents.
assert_eq!(Decimal::from_atomic(1_u32.into(), 8).to_string(), "0.00000001");

// Ordering is numeric, not structural.
assert!("0.00059859".parse::<Decimal>()? < "0.0006".parse::<Decimal>()?);

// 2^256 - 1 atomic units, read with 18 decimals, is exact.
let wei: Decimal = "115792089237316195423570985008687907853269984665640564039457\
.584007913129639935"
    .parse()?;
assert_eq!(wei.to_atomic_be_bytes::<32>(18)?, [0xff; 32]);
# Ok(())
# }
```

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | none | depends only on `bigdecimal` and `num-bigint` |
| base | `base` | re-exports [`Decimal`] as its amount value and [`Error`] as `DecimalError` |
| indexing | `indexing` | store and re-parse the canonical text of every movement, fee, and output |
| chains | `chain-bitcoin`, `chain-ethereum`, `chain-solana` | convert to and from `Satoshi`, `Wei`, and `Lamport` |

Design notes: [`ARCHITECTURE.md`](../../ARCHITECTURE.md),
[`docs/adr/0029-wrap-a-proven-decimal-core.md`](../../docs/adr/0029-wrap-a-proven-decimal-core.md),
[`docs/INDEXING.md`](../../docs/INDEXING.md).

[`bigdecimal`]: https://docs.rs/bigdecimal
