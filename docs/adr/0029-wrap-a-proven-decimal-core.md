# ADR-0029: Wrap a proven decimal core in `packages/decimal`

## Status

Accepted

## Date

2026-09-23

## Context

Every amount in the SDK — a movement, a fee, an output, a send request, an
HTTP body, a stored row — is one `Decimal`. It lived in `sdk/base/src/decimal.rs`
as 695 lines of hand-written arithmetic over `{ coefficient: BigInt, scale: u32 }`:
a hand-rolled normalizer, a hand-rolled cross-scale magnitude comparison, a
hand-rolled rescale, a hand-rolled parser, and a hand-rolled formatter. The
file was 195 lines past the 500-line limit, and roughly 45% of its public
surface (`new`, `coefficient`, `parts`, `from_parts`, `checked_add`,
`checked_sub`, `DecimalParts`, `DecimalSign`, `DecimalErrorKind`) had no
caller anywhere in the workspace.

The subtle parts are the ones nobody wants to own: ordering two values with
different scales without materialising `10^(scale difference)`, and stripping
trailing zeros so that one value has one spelling. Both are load-bearing.

The hard requirements, each verified against current source:

| | Requirement | Where it comes from |
|---|---|---|
| R1 | magnitude at least `2^256 - 1` | `Wei([u8; 32])`, `Wei::atomic()` is `decimal_with(0)` |
| R2 | scale at least 18 | ERC-20 `decimals` is chain data |
| R3 | exact only: no rounding, no inexact division | exact-money requirement, ADR-0005 |
| R4 | negatives representable, only so they can be rejected | `validate_amount` |
| R5 | `FromStr ∘ Display` is the identity on canonical text | `sdk/indexing/redb/src/amount_record.rs` and `sdk/indexing/postgres/src/row.rs` both re-assert `to_string() == encoded` on every read |
| R6 | plain positional output, never exponential | one satoshi is `0.00000001`, one lamport is `0.000000001` |
| R7 | numeric `Ord` across unequal scales | history ordering, batch validation (`amount <= Decimal::zero()`) |
| R8 | `Eq` by value, `Hash` consistent with it | value type in keys and assertions |
| R9 | no panic on hostile input | HTTP bodies reach `parse::<Decimal>()` |
| R10 | no Serde | amounts cross every boundary as text |

R5 is the sharpest: both repositories re-parse stored text and compare it to
the stored bytes, so any change to `Display` would make existing rows
unreadable.

## Decision

Move the type into its own package, `packages/decimal`, and wrap
[`bigdecimal`](https://docs.rs/bigdecimal) (arbitrary-precision decimal over
`num-bigint`) as the arithmetic core, keeping parsing and formatting as our
own code.

`bigdecimal` owns what was hand-rolled and subtle:

- `normalized()` — trailing-zero removal, with the wrapper clamping the scale
  at zero so `100` stays `100` rather than becoming `1E+2`;
- `Ord`/`PartialEq` — numeric comparison across unequal scales, implemented
  without materialising the power of ten (it compares decimal digit counts and
  then digit by digit), which the superseded code did materialise;
- `with_scale()` — exact rescaling, which `to_atomic` now uses for both the
  multiply and the exactness check (`rescaled != original` means digits would
  have to be rounded away);
- unbounded magnitude and scale, so R1 and R2 hold with headroom and no
  conversion gains a new overflow failure.

The text layer stays ours, in `packages/decimal/src/notation.rs`:

- `bigdecimal`'s own `Display` switches to exponential notation after five
  leading zeros (`EXPONENTIAL_FORMAT_LEADING_ZERO_THRESHOLD`, a build-time
  environment knob) and would print one satoshi as `1E-8`, violating R5/R6;
- `bigdecimal`'s own `FromStr` accepts exponents, which the SDK rejects.

Writing both keeps stored text independent of any upstream change or build
environment, which is the point of R5.

The public API keeps every used operation with its current signature —
`zero`, `from_atomic`, `scale`, `is_zero`, `validate_amount`, `to_atomic`,
`to_atomic_u64`, `to_atomic_be_bytes::<N>`, `From<u64>`, `FromStr`, `Display`,
`Ord`, `Eq`, `Hash` — and drops the unused surface. The four error kinds
collapse into one `decimal::Error` carrying the same messages; no caller ever
matched a kind.

`sdk/base` depends on the package and re-exports `Decimal` and
`Error as DecimalError`, so `base::Decimal` keeps working for every existing
user and no chain, indexing, wallet, or HTTP call site changes.

### Bounded input

`FromStr` rejects text longer than `Decimal::MAX_LEN` (512 bytes). The
superseded parser was unbounded: a 1 MiB request body produced a scale near
`10^6` and a conversion over a million-digit power of ten.

512 cannot reject a legitimate amount. The widest amount any supported chain
can express is `2^256 - 1` atomic units — 78 decimal digits. The widest
`decimals` an ERC-20 or SPL token can declare is 255, because both encode it
in one byte. The longest legitimate spelling is therefore 78 whole digits, a
point, and 255 fractional digits: 334 characters, 335 with a sign. 512 leaves
room for that plus leading zeros, and keeps every parse, comparison, and
conversion linear in a few hundred digits.

`from_atomic(units, decimals)` stays unbounded because `decimals` is asset
metadata fixed at composition, not caller input; parsing is the only untrusted
entry point.

## Alternatives considered

### `rust_decimal`

Rejected on R1. Its maximum is about `7.92e28`, four orders of magnitude of
*digits* short of `2^256 - 1` (about `1.16e77`), and it panics on a scale
above 28. It cannot hold one wei-precise ETH balance.

### `fixed`

Rejected on R3. It is binary fixed-point: `0.1` is not representable, so
exact base-10 money is impossible by construction.

### `num-rational`

Rejected on shape. A ratio has no scale, no canonical decimal spelling, and no
trailing-zero rule; every requirement from R5 to R7 would still be ours.

### `ruint::aliases::U256` (or `primitive-types::U256`) as mantissa plus a `u32` scale

Seriously considered, and rejected on two counts.

*It leaves more hand-rolled arithmetic, not less.* A fixed-width integer gives
proven addition and division but no scale, so normalization and cross-scale
comparison would still be ours — exactly the code this decision exists to stop
owning.

*It has no headroom.* The mantissa would cap at `2^256 - 1`, which is precisely
the largest legitimate atomic count, leaving nothing for an intermediate. The
`to_atomic` multiply by `10^(decimals - scale)` would gain an overflow failure
that does not exist today, and `FromStr` would start rejecting values the
current parser accepts. It is otherwise attractive — `ruint` is already in
`Cargo.lock` through `alloy-primitives`, it is `Copy`, and it allocates
nothing — and it remains the natural choice if this type ever needs to be
`Copy`.

### Keep the hand-rolled implementation and only split the file

Rejected. Splitting 695 lines into two files satisfies the line limit and
changes nothing about who owns the ordering and normalization logic.

## Consequences

- Stored text and API JSON are unchanged. Both repositories' canonical-text
  assertions keep passing, and the differential test below proves character
  equality against the superseded implementation.
- One new package in `Cargo.lock`: `bigdecimal`. Its other dependencies
  (`num-bigint`, `num-integer`, `num-traits`, `libm`, `autocfg`) were already
  there. `sdk/base` drops its direct `num-bigint` and `num-traits`
  dependencies.
- `Decimal` is no longer `const`-constructible (`scale()` was a `const fn`);
  no caller used it in a const context.
- `Decimal`'s `Debug` now prints `Decimal(1.23)` instead of the struct's
  fields.
- Hostile parse input is bounded for the first time.
- The type still allocates and is not `Copy`, exactly as before.

## Validation requirements

A differential test (`packages/decimal/tests/legacy_parity.rs`) carries the
superseded implementation verbatim as a reference and asserts the new one
agrees on:

- parse acceptance and the exact rejection message;
- `Display` output, character for character;
- `FromStr ∘ Display` identity;
- `scale()` and `is_zero()`;
- `Ord` over random pairs, with antisymmetry and agreement with `Eq`;
- `to_atomic`, `to_atomic_u64`, `to_atomic_be_bytes::<32>` and
  `validate_amount`, values and messages, at 0, 1, 6, 8, 9, 18, 36, and 255
  decimals;
- `From<u64>` and `from_atomic`.

The corpus covers one satoshi, one lamport, one wei, `2^256 - 1` at scale 0,
8, 9, and 18, `u64` boundaries at each chain's decimals, trailing- and
leading-zero families, 30- to 50-digit magnitudes, negatives, every literal
the superseded tests used, every literal the storage and chain crates spell
out, and 20,000 reproducibly generated literals.

The one deliberate divergence, text longer than `MAX_LEN`, is pinned by its
own test.

## References

- `packages/decimal/README.md`
- `packages/decimal/tests/legacy_parity.rs`
- `sdk/indexing/redb/src/amount_record.rs`
- `sdk/indexing/postgres/src/row.rs`
- `sdk/chains/ethereum/src/native/amount.rs`
- [`bigdecimal`](https://docs.rs/bigdecimal)
- `docs/adr/0005-use-checked-u64-lamports-for-native-sol.md`
