# 055 — Implementing the relational schema

Status: implemented. The schema agreed in conversation
(`schema/sdk.sql`, `schema/server.sql`) on top of 048–054. Replaces
`sdk/indexing/src/storage/sqlite/schema.sql` and its models.

Superseded on the balance question by 057 (running totals in the ledger).

## What changes, in one breath

- `block` becomes a real table; every block reference is an FK to it. The
  4× denormalized BlockRef copies disappear.
- `wallet` (wallets crate: identity, keys, metadata, birthday, archived_at)
  and `wallet_index` (indexing: wallet × asset → progress cursor + folded
  balance) replace the welded `wallet`/`watch` rows. Unwatch deletes the
  cursor row; facts stay.
- IDs are canonical `<chain>:<network>:<native>` strings; joins are
  single-column.
- Purge is `DELETE` of orphan blocks (CASCADE) plus cursor/checkpoint
  cleanup; the old `Purging` port is already merged into `IndexStore` (054).
- Finality stays out of the schema: a chain constant at indexer construction
  (Bitcoin depth), or the node (Ethereum, Solana). `network_checkpoint`
  records where the line is verified to.

## Port changes (`sdk/indexing/src/storage/`)

- `WatchStore::add(Vec<Watch>)` no longer owns registration rows. It inserts
  `wallet_index` rows and expects the `wallet` row to exist (FK); payments
  files the wallet first (below). A watch whose wallet row is missing is a
  `Conflict`.
- `WatchStore::all(network)` becomes the join: `wallet JOIN wallet_index`
  (active cursors only), reading id, address, asset, birthday, cursor —
  never key material. `WatchProgress` = { id, address, asset, birthday,
  indexed_through: Option<BlockRef> }.
- `WatchStore::forget` deletes the cursor row (unwatch). Re-watch resumes
  from the wallet's birthday; the engine re-inspects from there.
- `IndexStore::commit` writes, per batch: block rows insert-or-absent /
  verify-if-present (hash, position, height, timestamp, parents — a
  mismatching existing row is the existing terminal refusal); transactions,
  memberships, ledger rows, output creates/spends — all FK into block rows;
  cursor updates with the existing `Progress` CAS; checkpoint move with the
  existing expected-checkpoint CAS. `purge(network)`: delete that network's
  blocks (CASCADE takes transactions, memberships, ledger), delete its
  `wallet_index` rows and checkpoint. Wallet rows stay.
- `OutputStore::list` / `TransactionStore::locate` keep their ports; their
  SQL reads the new shapes (spend references, `transaction.id`).
- `Inspection` unchanged in role; `caught_up` now compares cursor block
  position against the checkpoint block's position.

## Wallets crate: the repository phase

Phase 1 (with this schema): indexing's shipped sqlite still writes `wallet`
rows; the schema already separates them.

Phase 2 (follow-up): `WalletRegistry` moves out of indexing into
`sdk/wallets`, as its own `storage/` module that mirrors indexing's storage
layout one-for-one:

```text
sdk/wallets/src/storage/
    mod.rs            the WalletRepository port + Arc forwarding impls + re-exports
    sqlite/           feature `sqlite`: the wallet table, statements, schema step
    memory/           feature `memory`: the same port on maps
sdk/wallets/tests/contracts/   shared suites, run against both stores
```

- The port is `WalletRepository` (`register`, `wallet(id)`, `wallets()`,
  `active()` — the non-archived rows payments loads at startup, `archive`);
  `birthday` and `archived_at` live here, as do keys — `sdk/wallets` still
  imports only `base` + `packages`, never indexing, either direction.
- Storage mechanics come from `packages/sqlite` (045), the same pool the
  indexing store shares by path; one file, two owners' tables.
- Factories create (`tag + pk → Wallet`); the repository restores
  (`row → Wallet`). Both live in the wallets crate; payments holds each
  where 053's slot wiring says so — the wallet-registry slot now comes from
  the wallets crate's store, not indexing's.

## payments: startup loading (053 container)

At `start()`, after composition:

1. Read every non-archived wallet (`WalletRepository` / today's registry).
2. For each, if no `wallet_index` row exists: `watch(id, holder, Fresh)` —
   the engine stores the wallet's birthday on the cursor's implicit range.
   If a cursor exists: nothing to do; `WatchStore::all` already restores it.
3. `changes()` and the rest of 053 are unaffected by the schema.

## Chain-side constants

- `IndexerParams::CONFIRMED_DEPTH` (Bitcoin) stays the finality constant;
  Ethereum/Solana ask their node. No schema involvement (as designed).
- `POLL`/`schedule` (054) unchanged by this work.

## SQLite implementation (`sdk/indexing/src/storage/sqlite/`)

- `schema.rs` loads `schema/sdk.sql` via `include_str!("../../../../../../schema/sdk.sql")`
  and fingerprints that text; the server's loader adds `schema/server.sql`
  the same way. `models/schema.sql` and `sqlite/schema.sql` are deleted.
- `models/`: one file per table, as today. `block.rs` is new (row + insert-
  or-verify). `watch.rs` becomes `wallet_index.rs` (cursor upsert/CAS,
  delete-on-unwatch, the join read). `wallet.rs` keeps key-material
  statements behind the registry port until phase 2. `ledger.rs`,
  `transaction.rs`, `output.rs`, `transaction_address.rs` switch to string
  ids and FK columns.
- Purge implementation shrinks: one `DELETE FROM block WHERE chain/network`,
  plus cursor and checkpoint deletes — cascades do the rest.
- Inherent reads (053): `balance(of)` = cursor row's `balance`, gated by
  `Inspection`; `history(of, page)` walks `history_entry` from the
  address's memberships.

## Memory store

Same shapes on maps: blocks keyed by id with (chain, network, position)
uniqueness enforced in the type; cursors per (wallet id, asset). Contract
suites run against both stores as today.

## Contract tests to add/update

- Block insert-or-absent; conflicting existing block → terminal refusal.
- `all(network)` returns the join; archived wallets and unwatched cursors
  absent.
- Unwatch deletes the cursor; re-watch resumes from birthday (the existing
  backfill scenario, re-sourced).
- Purge: network rows gone, other networks and wallet rows intact
  (the existing isolation scenario).
- Restart: checkpoint + cursor hashes verified against block rows.
