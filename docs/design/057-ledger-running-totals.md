# 057 — Balances as running totals in the ledger

Status: implemented (with the forward-only, block-position and in-block
order decisions below). Amends the implemented 055. One idea: the balance lives
in the ledger rows, not in a folded cache on the cursor.

## The change

`ledger` rows carry the signed change and the running total:

```sql
diff   TEXT NOT NULL,  -- Signed exact atomic change (negative = out).
total  TEXT NOT NULL,  -- Running signed total after this row.
```

- `direction` dies — the sign of `diff` is the direction. One column less,
  one concept less.
- `wallet_index.balance` dies — the cursor holds progress only.
- `kind` stays (movement / fee / withdrawal, and the movement vocabulary).

## Why

- **Single source of truth.** The total is derived from the same rows that
  are the facts; there is no second number that can drift from them.
- **Repair-proof.** Running totals are path-dependent, and repair deletes
  the orphan *suffix*: block 600 orphaned means its whole descendant branch
  goes. Retained rows only ever summed retained rows — after a suffix
  delete, the new tail's total is already correct. Totals are never
  rewritten; replay re-appends from the fork point. (This is the case that
  defeats per-row totals under middle deletes — and middle deletes do not
  happen in chain repair.)
- **Reads.** Balance = the `total` of the latest ledger row for
  (address, asset): a tail read on `ledger_address_asset`, O(log n) seek.
  The unwatched-address case (facts retained, cursor deleted) reads the
  same way — the fold fallback disappears.
- **Atomicity unchanged.** Each commit writes the movement and its total in
  the same transaction; identical retries verify identically.

## What changes, per owner

- `schema/sdk.sql`: the two ledger columns; `wallet_index` loses `balance`;
  header comment states the balance rule (latest retained row; repair never
  rewrites totals).
- SQLite `models/ledger.rs`: the insert computes `total` = previous
  retained row's total + `diff` (zero when none) inside the commit
  transaction; the row's identity check covers it.
- SQLite `models/wallet_index.rs`: drops the balance write from the cursor
  upsert.
- Inherent `balance(of)` read: tail-row `total`, gated by `Inspection`;
  pending overlay unchanged.
- Memory store: per-(address, asset) movement list; balance = last element.
- Contract suites: totals accumulate per row; balance read after each
  commit; after a simulated suffix delete the new tail is already right;
  unwatched address still answers from retained rows.

## Not changing

- `output` (the spend-reference repair is independent of ledger totals).
- `wallet_index` otherwise (birthday, cursor, CAS semantics).
- The commit contract, `Inspection`, purge/rescan.

## Rollout

1. Schema edit + sqlite ledger/wallet_index models + balance read. Gates.
2. Memory store + contract suites. Gates.

## Decisions at implementation

- Per-holding history is forward-only. A row at or before the holding's tail
  (backfill, a new watch's birthday, earlier counterparty rows) deletes the
  holding's rows from that block position on; the batch's rows from there are
  appended again. A new watch deletes its holding's rows from its birthday.
  A watch inspected through deleted rows the batch did not carry is rewound
  below them (same transaction). Totals are never rewritten in place.
- `ledger.block_position` orders the tail read:
  `(address_id, asset, block_position, movement_order, id)`.
- `movement_order` = 2 x movement index + side (credit 0, debit 1), so a
  self-transfer's credit sorts first and a zero amount keeps its side; a fee
  is `i64::MAX`, a withdrawal 0; ties break on `id`.
- `amount` and `direction` are replaced by signed `diff`. An embedded
  (Bitcoin) fee row keeps `diff` = −fee for history and adds 0 to `total`.
- A re-presented row is verified on every column but `total`.
- Follow-up: ledger rows are written only for watched (address, asset) pairs
  from their birthday; memberships and transactions only for addresses watched
  at that block. Unwatch deletes the watch's facts atomically with its
  `wallet_index` row: ledger rows, outputs, and — when no other asset of the
  address is watched — memberships, tombstones and transactions left
  unreferenced. The forward-only wipe now only touches a watch's own rows.
