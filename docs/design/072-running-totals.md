# 072 — Running totals on mined legs

Status: built. Deviations: see "As built" at the end.

## Why

A balance is summed from every leg of an index on every read (amounts are
exact u256 decimals, so not in SQL): the wallet list, the balance route and
every trigger round scan the whole leg history, and grow with it.

## Rule

- A **mined** leg (`block` set) carries `total`: the index's balance (address,
  asset) right after this leg, exact decimal text — the previous live mined
  leg's `total` plus this leg's signed amount. It never changes after it is
  written.
- A **pending** leg (`block` NULL) has no `total`. It is replaced by the mined
  leg when its transaction is mined (which gets a total then), or deleted when
  it expires or is refused.
- Order within an index: block position, then transaction order, then leg
  index; a block's rewards come after its transactions.
- Writes are in order: an index only moves forward and a reorg archives from
  the tip, so the legs an archive drops are always an index's newest; the next
  leg continues from the last live one. Nothing is rewritten.

## Schema

`leg` gains `position INTEGER` (the block's position; NULL while pending) and
`total TEXT` (NULL while pending), `CHECK ((block IS NULL) = (total IS NULL))`,
and an index on `(address, asset, position)` for "the last live leg of an
index at or below a position". No migrations: a fresh database.

## Writes

`BlockStore::insert` (the server's `keep`): for each index a block applies to,
read the total of its last live mined leg below the block once (0 when none),
then add each of its legs of the block in order. Mining a pending transaction
(`upsert_mined`) writes the mined legs this way. Archiving (`delete`) changes
no leg: archived legs drop out of every read by the live-block filter. A block
revived after an archive gets its facts written again the same way.

## Reads

- Balance now: the `total` of the index's last live mined leg (0 when none).
- Balance at block `H` (final = `finality_at`): the last live mined leg at or
  below `H`.
- Spendable: balance now minus the index's pending outgoing legs (spent,
  burned, fee; `block` NULL) — a few rows, summed in Rust.

The balance route, the balance model (`find_by_ids` / `find_signed_by_ids`),
the trigger round's spendable read and the wallet list use these lookups; the
wallet list reads wallets, indexes and each index's latest total in one query
(a view where it helps) — no ids collected, no legs scanned.

## Guard

A test recomputes every index's totals from the leg amounts and compares, over
sequences with reorgs, pending sends mined and expired, and a revived block.

## As built

- Schema: `leg` gains `wallet_index_id` (FK, `ON DELETE CASCADE`),
  `position`, `seq` and `total`. All four are NULL exactly while pending
  (CHECKs). A mined leg is kept for its index only, and deleting the index
  (or its wallet) deletes them. 071 kept them "no longer read"; now they
  just go, and a re-created index re-scans from its new start. Pending legs
  stay keyed by address: they have no index and no total.
- `seq`: per index, 1 + the highest it has ever kept, archived legs
  included, so it is never reused. `UNIQUE (wallet_index_id, seq)` refuses a
  second writer of the same step. Within a block it follows transaction
  order, then leg index, rewards last. An index's legs order by
  `(position, seq)`, on index `leg_wallet_index (wallet_index_id, position,
  seq)`. `leg_address (address, asset)` stays for the coin and pending
  reads. No views.
- Races: a block's facts are written in one `Db::write` (`BEGIN
  IMMEDIATE`), so the read of the previous leg and the insert are atomic and
  writers are serialized. The `seq` constraint makes a lost update fail
  rather than land.
- Writes: `keep` calls `transaction::db::insert_block` once. That call
  deletes the block's legs of the applying indexes (a block kept again,
  revived or re-applied, writes them again), then writes each transaction by
  `order` (`upsert_mined`, private), then the rewards by index. Each leg
  goes through `leg::db::insert_mined`: its total is the index's last live
  leg at or below the block (`find_last`, legs of this block written before
  it included) plus its signed amount, and its seq is the index's highest
  plus 1. No running-sum struct. Archiving changes no leg.
- Reads: the last leg is `find_last` (`WHERE wallet_index_id = ? AND
  position <= ? ⋈ live block ORDER BY position DESC, seq DESC LIMIT 1`, one
  seek). Its `total` is the balance there.
  - One balance definition: `balance` / `balance_confirmed` of
    `WalletIndex`, computed in SQL (`index_columns!`).
    `WalletIndex::spendable()` is `balance_confirmed` minus its pending
    outgoing legs (spent, burned, fee; no block), summed in Rust.
  - `models/balance.rs` and `models/wallet_index_balance.rs` are deleted.
    The trigger round reads `wallet_index::Selection`: the `WalletIndex`
    models of one network at the round's confirmed tip, keyed by id.
    `wallet_index::db::select` / `find_by_ids` / `find_by_target` build it
    with one query for the indexes and one for their pending legs only;
    the round's `Balance.spendable` is now `spendable()`, same meaning.
    Kept: a select loads pending legs only, not each index's mined
    history; the wallet reads load every leg.
  - `leg::db::can_spend` (pending-send check) is the same definition at
    the last block: the last live leg's total minus pending outgoing.
  - Gone: `find_signed_by_ids`, `leg::db::find_live`, `find_spendable` and
    `usecase::history::balance` (a pass-through).
- Read model. The models are the API's answer, with no API structs:
  - `models::Wallet { id, chain, network, address, keyed, tags, indexes }`.
    `indexes` is keyed by asset id. `keyed` is computed in SQL (`secret IS
    NOT NULL`). `curve` and `secret` are `serde(skip)`, and the wallet reads
    never select them (the key store reads keys separately).
  - `WalletIndex { id, wallet_id, asset, state, reason, requested_from,
    birthday, indexed_through, behind_tip, finality_at, balance,
    balance_confirmed, legs }`.
  - `Leg` as stored, in (position, seq) order, pending legs last (no total).
- `wallet::db::find_at_tip(id, tips)` and `list_at_tip(tips, after,
  limit)`: one statement for the wallet(s) left-joined to their indexes,
  with every computed column, then one ordered statement for their legs,
  keyed by the page's wallet ids. They are grouped under each index in the
  model.
  - `tips` is JSON, per `{chain}.{network}`, of `{tip, confirmations}`. It
    is bound once and read with `json_each` in a CTE, joined on the wallet's
    chain and network, so one statement serves every network.
  - Computed per index: `finality_at = tip - confirmations`, `behind_tip =
    max(tip - indexed position, 0)`, `balance` = the last live mined leg's
    total (any block, `'0'` without one), `balance_confirmed` = the same at
    or below `finality_at`. Each is one seek on `leg_wallet_index`.
  - A network missing from `tips`: null `behind_tip` and `finality_at`,
    `balance` still over every live block, null `balance_confirmed`.
  - The API builds `tips` from `Payments::stats()`: confirmations = tip −
    the confirmed tip.
- The index columns are one SQL fragment (`wallet_index::index_columns!`),
  shared with the trigger read (`wallet_index::db::select`, which binds no
  tip and the round's confirmed position).
- `Tips::from_stats(payments.stats())` builds the per-network tips.
- API routes reply with models and call the model directly: removed the
  wrappers `api::wallets::wallet` (read a wallet), `api::wallet_indexes::index`
  (read the whole wallet, then pick one asset; now
  `wallet_index::db::find_at_tip` by index id), `api::tips` (now
  `Tips::from_stats`), and the triggers' API `Trigger` struct with its
  `From<models::Trigger>` (`models::Trigger` is serialized as is, with
  `group_id` as `group`; JSON unchanged).
- Gone: `WalletWithIndexes`, `wallet_index_progress.rs`, `fill_progress`,
  `find_with_indexes`, `wallet::Indexed`, `usecase/wallets.rs`.
- API changes (asked for):
  - `GET /v1/wallets/{id}/balance` is gone.
  - Wallet bodies (read, create, list) are `models::Wallet`; index bodies
    (create and patch index) are `models::WalletIndex`. Balances are exact
    atomic text; the UI scales them by the asset's decimals.
  - `GET /v1/wallets` takes `cursor` (the last wallet id) and `limit`
    (1–1000, default 100), and answers `next_cursor`. The UI loads every
    page.
  - One id format: the model id `{chain}.{network}.{address}` in routes,
    bodies and the UI. `chain:network:address` is gone.
  - `docs/openapi.json` and `ui/src/api/schema.d.ts` are regenerated.
- Guard: `tests/storage.rs`
  `running_totals_match_the_amounts_through_reorgs_and_sends`. It
  recomputes every live leg's total per index from the amounts in 072's
  order (transaction `order`, not `seq`). It checks against that: the leg's
  index, the balance read, `wallet_index::db::find_by_ids` at every bound, and
  `wallet::db::list_at_tip` at every final block (`balance`,
  `balance_confirmed`, `finality_at`, and the legs' totals in order). It runs over several indexes in one block, rewards out of index
  order, a pending send mined with another fee, one that expires, a reorg,
  a revived block, and a refused duplicate `seq`. The sibling
  `two_pending_sends_of_one_index_land_in_the_totals` covers two pending
  sends of one index: (a) mined in one block out of list order, (b) mined
  in two blocks, (c) one mined and one expired, with spendable before and
  after. The pipe test seed writes index, position, seq and total.
