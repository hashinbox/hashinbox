# 087: Index pointers and adjustments as positions

Status: proposed (2026-11-19).

## Problem

Several tables point at `block` with a foreign key. Two kinds of pointer are mixed:

- **Facts of a read block:** `leg.block` for its transactions' legs and its rewards, and
  `transaction.block`. They belong to the block: when a reorg or a prune deletes it,
  `ON DELETE CASCADE` deletes them too.
- **Places the server reached without reading the block:**
  - progress pointers: `wallet_index.indexed_at` ("read through here"),
    `transaction_index.indexed_at` and `transaction_index.found` ("found in here");
  - adjustment legs (kind 6): a settle's balance correction and a Bitcoin jump's write-off, on
    `leg.block`.

  The block is often never read: a settle learns it from the node (a balance read, or the
  `Confirmer`'s receipt).

Because these are foreign keys, a settle must invent a block row with no facts (`Block::empty`,
`keep_empty`) for them to name. That causes:

- empty blocks that look like read ones (S2's reverted check found no transactions in one);
- prune has to keep every block an index still names (the `NAMED` subquery in
  `apps/server/src/models/block.rs`);
- the index views and models join `block` to read their own place
  (`wallet_index_with_balance`, `transaction_index_with_blocks`, the aliases in
  `models/transaction_index.rs`);
- `Chained` needs a header path (`add_header`, `Fill::Empty`) only for these rows.

## Decision

Places the server reached without reading the block become block **positions**; facts of a read
block keep their foreign key. No block row is ever created without its facts.

| column | today | after |
|---|---|---|
| `wallet_index.indexed_at` | FK to `block` (id) | `INTEGER`: the position read through |
| `transaction_index.indexed_at` | FK to `block` (id) | `INTEGER`: the position read through |
| `transaction_index.found` | FK to `block` (id) | `found_at INTEGER`: the position it is mined in |
| `transaction_index.found_reverted` | `BOOLEAN` | gone: reverted is the transaction row's own state |
| `leg.block` of an adjustment (kind 6) | FK, cascade | `NULL`; its `position` holds the place |
| `leg.block` of any other mined leg | FK, cascade | unchanged |
| `transaction.block` | FK, cascade | unchanged (rows are taken back explicitly first) |
| `transaction.position` | (the view joined `block`) | `INTEGER`: its block's position; NULL while pending |
| `block.parent` | FK, set null | unchanged |

- **No hash on a position.** A reorg is caught by the block store (a block that does not link to
  the live block below it is refused) and undone in the same write: `rewind` moves every index
  at or above the reorged position back, and deletes the adjustments there. Nothing ever names a
  reorged block. Below the line, where blocks may be pruned, nothing reorgs.
- **No height on a position.** Only Solana's expiry needs one (a transaction can land only up to a
  block *height*, `lastValidBlockHeight`, and slots skip). The indexer knows nothing of expiry: it
  reads a transaction index on until it sees the transaction or the index is deleted. The broadcast
  job judges expiry by the height the `block` table already stores (below, **Expiry**).
- **Tips are one number; confirmed is computed one way.** `base::Tips { position }` is the chain's
  newest block: Ethereum `eth_blockNumber`, Bitcoin `getblockcount`, Solana `getSlot` (at
  `confirmed` commitment). No `finalized` call. The confirmed position is `position -
  confirmations` (saturating, `Tips::confirmed`), `confirmations` the network's config
  (`Chained::confirmations`): the indexer's line (`Line::Final`, the block after it), the gap
  closes (settles, `Confirmer`s), the transaction index's `status()` and `network_index.confirmed`
  all use it. Ethereum's default `confirmations` is 64 (about two epochs, where `finalized` sits),
  Solana's 32, Bitcoin's `confirmation_depth - 1`.
- **Adjustments are not facts of a block.** They correct an index's running total at a position;
  nothing about them is in the block. The cascade no longer removes them; `rewind` does, by
  position, in the reorg's write.

## Schema

```sql
-- wallet_index
indexed_at INTEGER,          -- the position read through; NULL: nothing read yet
CHECK (state IN (0, 1, 2))

-- transaction_index
indexed_at INTEGER,          -- the position read through; NULL: nothing read yet
found_at   INTEGER,          -- the position it is mined in; NULL until mined
-- state, only what the index itself does: 0 indexing, 1 paused, 2 no_data (given up);
-- found = `found_at` set, reverted = its transaction row's state; no state for expiry
CHECK (state IN (0, 1, 2))

-- leg: an adjustment has no block, but a position and its totals
CHECK (kind <> 6 OR block IS NULL)
CHECK ((position IS NULL) = (total IS NULL))   -- pending legs: neither

-- transaction
position INTEGER                             -- its block's; NULL while pending

-- network_index: written the first time the tip is known (no row: no tip yet)
tip       INTEGER NOT NULL                   -- the newest block's position
confirmed INTEGER NOT NULL                   -- tip - confirmations (saturating), written with it

-- transaction
broadcasted_at INTEGER                       -- Unix ms this server recorded it, before its first
                                             -- broadcast; NULL: not ours (was `sent_at`)
```

`wallet_index_progress` and `transaction_index_progress` index the new integer `indexed_at`; legs
keep their `(wallet_index_id, position, seq)` order, adjustments included.

A leg's id is built by its constructor in `models/leg.rs`, deterministic so a block read again
writes the same legs: an adjustment's is `{wallet_index_id}.adjustment.{position}.{leg_index}`.
The table no longer checks the id's shape (today's `CHECK (id = CASE kind …)` goes), nor the other
combinations the constructors already guarantee; only the two checks above stay, on what a
migration or a hand-written row could get wrong.

## Base

- `Progress.indexed_at: Option<u64>`: the position read through.
- `TransactionIndex.found: Option<u64>` (`found_at`) and `reverted: bool`, derived: a store
  fills it from its transaction row's state (the block was read, so the row is mined with its
  outcome; or the `Confirmer`'s answer marked it, `transaction::db::set_mined`). A found index
  stops reading (`Indexed::next` is `None` once found). `IndexState` keeps only what an index
  does (indexing, paused, no data): no found or reverted states.
- `TransactionIndex::status()`: found at or below the confirmed position is `Final`, or
  `Reverted` when reverted; found above it `Unknown`; not found, `NoData` is `GivenUp`;
  `Indexing` read through the confirmed position is `Missing`. No `Expired` status:
  `TransactionIndex::expired(height)` is `Missing` with `expires_height` below `height`, the
  sender's to judge (**Expiry**).
- The indexer never judges expiry: a transaction index not found in a block reads on.
- `Tips { position }`, `Tips::confirmed(confirmations)`, `Chained::confirmations()`;
  `Indexes::close(line, confirmed)` takes the confirmed position; `Stats.tip`, `Stats.confirmed`.
- The store's settles take a position, not a header (`WalletIndexStore::settle`,
  `TransactionIndexStore::settle`).

## What changes in code

- **Advance** (`wallet_index::db::advance`, `transaction_index::db::advance`): an index moves onto
  a block when it has read through its parent: `indexed_at >= parent_position AND indexed_at <
  position` (nothing lies between a block and its parent, so any position in that range means
  "read up to the parent"; it also covers a pointer left on a skipped slot or a settle position
  no block sits at). A transaction index found in the block sets `found_at` to its position;
  its outcome is its transaction row's.
- **Reorg** (`BlockStore::delete(network, from)`): a hard delete, in one write, one call per
  model, each on its own table only (only the usecase knows several), with `to = from - 1`,
  rows before the blocks so no block cascade reaches a transaction:
  ```rust
  transaction::db::delete_unbroadcast_from(connection, &network, from)?; // others' mined at >= from (broadcasted_at NULL); their legs by the leg.transaction cascade
  leg::db::delete_kind_from(connection, &network, from, leg::ADJUSTMENT)?; // adjustments at >= from (they have no block)
  leg::db::set_pending_from(connection, &network, from)?;              // the transaction legs left at >= from (only ours): block, position, seq, totals NULL
  transaction::db::set_pending_from(connection, &network, from)?;      // the mined left at >= from (only ours): block, position, order, reason NULL, state pending
  block::db::delete_from(connection, &network, from)?;                 // the blocks; their rewards by cascade
  wallet_index::db::cap_indexed_at(connection, &network, to)?;         // indexed_at = MIN(indexed_at, to)
  transaction_index::db::cap_indexed_at(connection, &network, to)?;
  transaction_index::db::clear_found_from(connection, &network, from)?; // found_at >= from: NULL (no state change)
  ```
  - `cap_indexed_at`: `indexed_at = MIN(indexed_at, to)`. No lookup of the block below: an index
    left at `to` reads on from the next block (advance's range), and one capped at or above its
    settle never reads the settle block again.
  - No soft delete: `block.reorged_at`, reviving a reorged block in `block::db::save`, prune's
    `delete_reorged` and the stats' `reorged` count go.
  - **Our own send** mined in a deleted block goes back to pending, not away: its hold on the
    funds must stay while it may land again in the new fork; its broadcast job owns it until
    final, expired or forgotten. `transaction.broadcasted_at` marks ours: set when its pending
    record is made (`transaction::db::save_pending`, before the broadcast), kept when mined
    (`upsert` never updates it); others' rows never have it. No ids are passed.
  - `leg::db::set_pending_from` touches transaction legs only: a block's rewards keep their block
    and go with it.
  - `transaction.block` stays `ON DELETE CASCADE`: the reorg leaves no transaction in a deleted
    block, and prune deletes only blocks with none, so the cascade never fires; keeping it needs no
    table rebuild.
  - No double count: a leg is mined (`position` set) or pending (`position IS NULL`), never
    both; `wallet_index_with_balance` counts pending outgoing legs against
    `balance_confirmed_spendable` and every pending leg in `balance_pending`.
  - **The reorg loop:** mined is not done until final. The broadcast job waits while a
    version is found above the final block (no resend, no bump), its spend the mined legs. A
    reorg puts the payment back to indexing (its index capped, `found_at` cleared) with its
    spend counted as pending legs; the next check finds it missing and sends the same bytes
    again, bumps it once `bump_after` is read, cancels it when no bump is left, as for any
    missing payment (design 084, Resolution). Mined again and final: done; the pending legs
    became its mined ones, nothing counted twice. The job's end deletes whatever version is
    still pending, with its legs.
  - `cap_indexed_at` keeps the birthday rule of the old rewind: an index capped below its
    birthday (a wallet index's `birthday`, a transaction index's `broadcasted_at`) is unset.
- **Keep** (`BlockStore::insert`, `keep` in `usecase/sdk/blocks.rs`): plain, one block at a
  time: `block::db::save` writes the row only (insert; the same id already kept is a no-op;
  another block at its position fails on the unique index `block_position`, "block {position}
  conflicts with the kept one"), then `facts` (the usecase) moves the indexes onto it and writes
  its transactions and legs. The store no longer reads the live blocks around a run to check
  links (`find_around`, `Blocks::place`, `check_links` go): with no reorged rows left, the unique
  index refuses a second block at a kept position, and the indexer's forward read starts at the
  last kept block, in a run `base::Blocks` already links (and keeps to one network), so a block
  that does not link onto what is kept never reaches the store.
- **Legs** (`models/leg.rs`): `leg::db::append(connection, &mut Legs, position)` reads each
  index's seq and totals at the position once, has each leg follow its index in order and
  inserts them; `facts`, the settle's adjustment and the write-off call it (`find_running` is
  private; a settle reads its balance with `leg::db::find_total`).
- **Transaction index advance:** two set-based updates, no decision in memory:
  `transaction_index::db::set_indexed_at(connection, block)` (every index the block applies
  to, one `UPDATE`), then `transaction_index::db::set_found_at(connection, block,
  &block.txids())` (those moved onto it whose transaction is in it, one `UPDATE … RETURNING
  id`: the ids whose legs the block keeps).
- **Expiry** (the broadcast job, `apps/server/src/usecase/workers/broadcast.rs`): in the check's
  read, the job reads the confirmed position (`network_index::db::find_confirmed`) and the
  height of the newest kept block at or below it (`block::db::find_height_at_or_below`); a
  version is expired when that height is above its `expires_height`, its index read through the
  confirmed position and its transaction not found (`TransactionIndex::expired`). No kept block
  there (or no tip yet): none expired, the job waits. It holds because reading a block stores
  its row, and prune deletes only below the line: Solana's prune line is the confirmed position
  (a chain that reads balances), its indexer line `tip - read_depth`, which config validation
  keeps at or below it (`read_depth >= confirmations`: devnet 150 and the default 1000 against
  32). A pruned block below `confirmed` only makes the height found lower: expiry never comes
  early. A version expired is, in one write, dropped: its pending row with its legs
  (`transaction::db::delete_pending`, the legs by its cascade) and its index
  (`transaction_index::db::delete`), so its spend is released. Another version still pending:
  watched on (a current version with no index was dropped so: only the job deletes one). None
  left and a bump left: a new version signed by its builder on a fresh blockhash a tenth up,
  recorded as any (`transaction_indexes::record`, its own funds check) in the drop's write, sent
  by the next run at once; short: nothing written, the job's end drops the version and fails
  `insufficient-funds`, outcome `expired`. No bump left: the job's end (one write) drops it and
  fails `expired`; its state keeps outcome, reason and every txid.
- **Settle:**
  - wallet index: writes the adjustment at position S (no block), sets `indexed_at` to S;
  - transaction index: sets `indexed_at`, and `found_at` from the `Confirmer`'s answer; its
    outcome goes on its pending transaction row (`transaction::db::set_mined`: position and
    state succeeded or reverted, no block), which a reorg's `set_pending_from` undoes;
  - Bitcoin jump: the write-off adjustment at its position, as the settle's.
  No block is kept by any of them.
- **Views:** `wallet_index_with_balance` reads `indexed_through` from `indexed_at` (no join to
  `block`), and its running totals from legs by position (adjustments have no block to join);
  `transaction_index_with_blocks` goes, the model reads the table.
- **Models:** `models/transaction_index.rs` loses its `block` aliases and `Option<Block>` fields:
  `indexed_at: Option<i64>`, `found_at: Option<i64>`, and its state. `Leg::adjustment` takes a
  position, not a block.
- **Prune:** `NAMED` and `delete_reorged` go. A block below the line is deleted when it holds no
  legs and no transactions; its facts go with it by cascade. Adjustments hold no block.
- **Chained** (`usecase/sdk/blocks.rs`): only full blocks; `add_header` and `Fill` go.

## Migration

`docs/design/087-migrate-1.sql`, in one transaction:

1. rebuild `wallet_index` and `transaction_index` (SQLite cannot drop a column with a foreign key:
   create, copy, drop, rename), copying `indexed_at` / `found` as the named block's position
   (`SELECT position FROM block WHERE id = …`); every state is the index's own (0, 1, 2: any
   other becomes 0), `found_at` kept, the transaction row's state as it is;
2. rebuild `network_index` with `tip` and `confirmed` NOT NULL (a row without a tip is dropped,
   written again by the indexer's next step); no transaction index changes state for expiry;
3. rebuild `leg` with the two checks, copying an adjustment with `block` NULL, its `position`
   kept, and its id rewritten to `wallet_index_id || '.adjustment.' || position || '.' ||
   leg_index`;
4. recreate the indexes and the views on these tables; drop `transaction_index_with_blocks`;
5. delete the blocks nothing holds any more: no legs, no transactions (the old empty blocks);
6. rebuild `block` without `reorged_at`, with the plain unique `block_position`, and
   `transaction` with its block's `position` and `broadcasted_at` for `sent_at` (ours: a pending
   row, or one a transaction index looks for: `sent_at`, else its broadcast job's `created_at`,
   else now; others NULL), without `CHECK (block IS NULL OR sent_at IS NULL)`;
7. `transaction_forgotten` is dropped: the broadcast job's state is the record of a payment
   that never landed (design 084, Resolution).

As run, the file first deletes the reorged blocks with their facts by hand (foreign keys are off,
nothing cascades; a transaction a transaction index looks for goes back to pending, as a reorg
does), so no later step trips on them, and creates the views (4) last, after every rebuild.

## What is deleted

- empty blocks altogether: `Block::empty`, `keep_empty` and its three callers (both settles and
  the transaction index's found block), `Chained::add_header` and `Fill`;
- `transaction_index_with_blocks` and the `block` aliases and joins in
  `models/transaction_index.rs`;
- the `NAMED` subquery of prune;
- the `block` join in `wallet_index_with_balance` for the index's own place;
- the height comparison in `TransactionIndex::status()`;
- soft delete of blocks: `block.reorged_at`, reviving, `delete_reorged`, the `reorged` stat;
- `find_below` in the reorg, and the "rewind back to a settle" clause;
- the store's link checks: `find_around`, `Blocks` (the server's collection), `Place`,
  `check_links`, `Block::is`, `Block::is_parent_of`, the per-block network check;
- `find_running`/`follow`/`insert` in the usecases (`leg::db::append`).
- `transaction_forgotten` (table and model): the broadcast job's state keeps a payment's outcome,
  reason and txids.

## Risks

- **The real database** needs three table rebuilds (`wallet_index`, `transaction_index`, `leg`);
  `leg` is the largest table (about 24 000 rows today). Back up first; check row counts and
  balances before and after.
- **Adjustments and reorgs:** the cascade no longer covers them, so the reorg deletes them; a test
  reorgs across a settle and checks the adjustment goes and the index settles again.
- **Our sent transaction in a reorged block:** `tests/check.rs` mines it above the final block,
  reorgs that block, and checks it is pending again with its hold, sent again with the same
  bytes, found again in the new fork and done once final, spent once; and, with no block in the
  new fork, sent again and bumped at `bump_after`, held throughout.
