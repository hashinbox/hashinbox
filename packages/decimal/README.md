# decimal

An exact base-10 amount of any size, for money that crosses a chain, a database
row and an HTTP body as text. It parses and prints one canonical spelling and
converts to atomic units without rounding.

## Usage

```rust
use decimal::Decimal;

# fn main() -> Result<(), decimal::Error> {
let amount: Decimal = "0.01500".parse()?;
assert_eq!(amount.to_string(), "0.015");

// Display units to atomic units: exact, or an error.
assert_eq!(amount.to_atomic_u64(8)?, 1_500_000);
assert!(amount.to_atomic(2).is_err());

// Atomic units back to display units.
assert_eq!(Decimal::from_atomic(1_u32.into(), 8).to_string(), "0.00000001");

// Ordering is numeric across unequal scales.
assert!("0.00059859".parse::<Decimal>()? < "0.0006".parse::<Decimal>()?);
# Ok(())
# }
```

## Notes

- The printed text is a storage format. The SQLite repository re-parses every
  stored amount and rejects one whose text differs from what `Display` prints,
  so changing the spelling makes existing database files unreadable.
