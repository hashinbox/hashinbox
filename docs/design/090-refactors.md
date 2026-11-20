# 090: Obvious refactors

Status: proposed (2026-11-20). An audit of the working tree (087 applied, uncommitted, with the
`usecase/blocks.rs` and `usecase/indexes.rs` moves). Behaviour stays the same. Cuts are in 089 and
are not repeated here. Every item was checked with grep. Line numbers are from the working tree.

- Already planned, not repeated below: `usecase/indexes/{wallet,transaction}.rs`, with
  `record/replace/restore/forget` out of `usecase/sdk/transaction_indexes.rs` and the
  `settle/restart` bodies out of `usecase/sdk/indexes.rs`. Items marked "(same pattern)" join it.
- Items that touch an 089 cut say so. If the cut lands, skip the item.

## Top 10

| # | Refactor | Where | Size | Approval |
|---|---|---|---|---|
| 1 | `save_pending` writes 3 models and checks funds inside the `transaction` model | `models/transaction.rs:301` | M | no |
| 2 | More store and worker code orchestrating models (same pattern as the planned split) | `usecase/sdk/indexes.rs:21`, `usecase/sdk/transaction_indexes.rs:83`, `usecase/workers/broadcast.rs:423,832`, `usecase/workers/trigger/mod.rs:156` | M | no |
| 3 | Models that read or write other models' tables | `models/network.rs:199`, `models/trigger_run.rs:136`, `models/transaction_index.rs:90` | S | no |
| 4 | One rule, two names, two styles: `advance` (raw `UPDATE…RETURNING`, joins `wallet`) and `set_indexed_at` | `models/wallet_index.rs:562`, `models/transaction_index.rs:192` | M | no |
| 5 | Vague `keep` / `apply` / `ours` on the block write | `usecase/blocks.rs:18,41`, `models/block.rs:208`, `models/transaction.rs:206` | S | no |
| 6 | Booleans that pick a branch or encode a stage | `broadcast.rs:116,241`, `usecase/wallets.rs:158`, `usecase/networks.rs:433`, `models/transaction.rs:368`, `models/wallet_index.rs:898` | S–M | no |
| 7 | Magic codes: leg kind 0–6, transaction state 0–2, task status 2; ints in the API | `models/leg.rs:84`, `models/transaction.rs:378,445`, `usecase/stats.rs:171` | M | no |
| 8 | List pages answer `{jobs,…}`, `{blocks,…}`, not `{items, next_cursor}` | 11 pages in `api/*.rs` | M | no |
| 9 | Stale names after 087: "facts", `birthday`, `transaction_index.broadcasted_at` (a position, not a time) | `init.sql:220,262`, `api/wallet_indexes.rs:116,138`, base | S–M | yes (base part) |
| 10 | Chain code copied 3×: `rpc_info.rs`, `key.rs` (Ethereum/Solana), `wallet.rs`, builder plumbing | `sdk/chains/*/src` | M | yes |

- Next by value: the two `Sqlite` store ports (`usecase/trigger.rs:16`, `usecase/tasks.rs:24`), the
  `WalletIndexer`/`TransactionIndexer` copy in base, `found` + `reverted` pairs, dead
  `Transport::replace`/`Clients::highest`.
- Top 9 and 10 touch `sdk/base`: ask first (core crate rule). `sdk/wallets` does not exist; nothing
  here touches `packages/crypto`.

---

## Server models (`apps/server/src/models`)

- **`save_pending` mixes three models and business logic** (top 1)
  - **Where:** `models/transaction.rs:301-339`.
  - **What's wrong:** the `transaction` model inserts its row, reads `wallet_index::db::find_spending`,
    runs the funds check (`Refused::Short`) and calls `leg::db::insert`.
  - **Fix:** the model only inserts the row; the funds check and the leg insert move to `record` in
    the planned `usecase/indexes/transaction.rs`.
  - **Size:** M. **Needs approval:** no.

- **`network::db::find_all` composes five models** (top 3)
  - **Where:** `models/network.rs:199-219`.
  - **What's wrong:** it calls `network_asset`, `endpoint`, `endpoint_route` and `network_index`
    `db::find_all` and stitches them.
  - **Fix:** the model reads its own rows; `usecase/networks.rs` composes them.
  - **Size:** S. **Needs approval:** no.

- **`trigger_run::db::insert` writes `triggers`**
  - **Where:** `models/trigger_run.rs:136-146`.
  - **What's wrong:** it also sets `triggers.last_run`.
  - **Fix:** `trigger::db::set_last_run`; the caller runs both in its write.
  - **Size:** S. **Needs approval:** no.

- **`transaction_index` reads two other tables and borrows a codec**
  - **Where:** `models/transaction_index.rs:90-112` (`read!` joins `transactions` and
    `network_index`), `:15` (imports `wallet_index::db::{code, state}`).
  - **What's wrong:** a model querying other tables; the index-state codec lives in
    `wallet_index` but serves both.
  - **Fix:** a view `transaction_index_with_state` (as `wallet_index_with_balance`); the codec in
    one `models/index_state.rs`.
  - **Size:** S. **Needs approval:** no.

- **`advance` and `set_indexed_at`: same rule, two shapes** (top 4)
  - **Where:** `models/wallet_index.rs:562-603`, `models/transaction_index.rs:192-218`.
  - **What's wrong:** both "move every index this block applies to onto it". One is raw
    `UPDATE…RETURNING` with a `wallet` subquery, the other diesel. Different names.
  - **Fix:** both named `set_indexed_at`, both diesel; the wallet one finds the ids first, then
    updates them.
  - **Size:** M. **Needs approval:** no.

- **`wallet_index` and `leg` carry no chain or network**
  - **Where:** `wallet_index.rs:543,577,761` (subqueries on `wallet`), `leg.rs:417-424`
    (`format!` id-range trick), `leg.rs:776-777` (joins `block` for the network).
  - **What's wrong:** every per-network statement reaches into another table or uses a string
    range.
  - **Fix:** `chain`, `network` NOT NULL columns with a `CHECK` on the id prefix, as
    `transaction_index` has (`init.sql:257-271`).
  - **Size:** M. **Needs approval:** no.

- **`block::db::prune` checks `leg` and `transaction`**
  - **Where:** `models/block.rs:458-482`.
  - **What's wrong:** `NOT EXISTS` subqueries on two other tables.
  - **Fix:** `usecase/workers/prune.rs` asks `leg::db` and `transaction::db` for the blocks they
    use, then `block::db::delete_unused(ids)`. Lower value; do last.
  - **Size:** M. **Needs approval:** no.

- **Vague `keep` / `apply` / `ours`** (top 5)
  - **Where:** `usecase/blocks.rs:18` (`keep`), `:41` (`apply`), `:42` (`ours`);
    `models/block.rs:208` (`Block::keep`), `models/transaction.rs:206` (`Transaction::keep`),
    `:322,336` (`ours`).
  - **What's wrong:** `keep` saves, applies and filters; `ours` is "indexes the block moved".
  - **Fix:** `blocks::save_run`, `apply_to_indexes`, `Block::retain_legs_of`,
    `Transaction::retain_legs_of`, `ours` → `advanced` / `indexed`.
  - **Size:** S. **Needs approval:** no.

- **Conversions named after a field**
  - **Where:** `wallet_index.rs:244` (`Row::progress` returns a whole `base::WalletIndex`),
    `transaction.rs:217` (`Transaction::transaction`), `leg.rs:296` (`Leg::leg`),
    `wallet_index.rs:201` (`WalletIndex::index`).
  - **What's wrong:** the names don't say "into base type".
  - **Fix:** `TryFrom<…> for base::…`, as the other models do.
  - **Size:** S. **Needs approval:** no.

- **Leg kinds as raw ints** (top 7)
  - **Where:** `leg.rs:136-140`, `:645,654,782,789,799`; `transaction.rs:621,657` (`IN (1, 2, 4)`).
  - **What's wrong:** only `REWARD` (`leg.rs:54`) and `ADJUSTMENT` (`:56`) are named.
  - **Fix:** a `LegKind` enum with diesel `ToSql`/`FromSql`, or named consts plus `OUTGOING`.
  - **Size:** S. **Needs approval:** no.

- **Transaction state as raw ints and a bool**
  - **Where:** `transaction.rs:378` (`if reverted { 2 } else { 1 }`), `:445` (`eq(0)`), `:469`
    (`state = 2`), `transaction_index.rs:71` (`Some(2)`); `set_mined(…, reverted: bool)` at `:365`.
  - **What's wrong:** `encode_state`/`decode_state` (`:67,:87`) exist, but writers bypass them.
  - **Fix:** `PENDING/SUCCEEDED/REVERTED` consts used by all; `set_mined` takes the state.
  - **Size:** S. **Needs approval:** no.

- **`read(…, fully_indexed: bool)` is a test-only switch on `format!` SQL**
  - **Where:** `wallet_index.rs:898-964`; `false` only from `#[cfg(test)] find_by_ids` (`:887`).
  - **What's wrong:** a bool in prod code for a test; `picked` (`:967`) is a vague name.
  - **Fix:** drop the flag (always fully indexed); the test reads the view itself; `picked` →
    `selector_sql`.
  - **Size:** S. **Needs approval:** no.

- **`set_state` has a dead `None` branch**
  - **Where:** `wallet_index.rs:645-670` (`asset: Option<&str>`).
  - **What's wrong:** both callers (`usecase/wallets.rs:247`, `usecase/sdk/indexes.rs:103`) pass
    `Some`; five args.
  - **Fix:** `set_state(connection, &WalletIndexId, state, reason)`.
  - **Size:** S. **Needs approval:** no.

- **`WalletIndex` holds the same value twice**
  - **Where:** `wallet_index.rs:133` (`indexed_through`), `:165` (`indexed_at`, skipped);
    `init.sql:299` (`wi.indexed_at AS indexed_through`).
  - **Fix:** one field, serialized as `indexed_through`.
  - **Size:** S. **Needs approval:** no.

- **`network_index.checked_at` nullable, always written**
  - **Where:** `init.sql:420`, `network_index.rs:22,48`.
  - **What's wrong:** the only writer (`save`) always sets it.
  - **Fix:** NOT NULL, `u64`.
  - **Size:** S. **Needs approval:** no.

- **`NetworkIndex::step` reuses the stored type as a delta**
  - **Where:** `network_index.rs:37-53`.
  - **What's wrong:** its counters mean "grew by" here, "total" when read; `step` is vague.
  - **Fix:** a small `IndexerStep` type for `db::save`.
  - **Size:** S. **Needs approval:** no.

- **`find_below(…, i64::MAX)` is `find_last`**
  - **Where:** `block.rs:413`; only caller `usecase/sdk/blocks.rs:27`.
  - **Fix:** `block::db::find_last(connection, network)`.
  - **Size:** S. **Needs approval:** no.

- **Blanket `allow(dead_code)` on whole `db` modules**
  - **Where:** `wallet.rs:209-212`, `trigger.rs:217-220`.
  - **What's wrong:** every function there has a caller by grep; the allow would hide new dead
    code.
  - **Fix:** drop both allows.
  - **Size:** S. **Needs approval:** no.

- **Orphan doc comment**
  - **Where:** `wallet_index.rs:819-824`.
  - **What's wrong:** `find_by_ids`'s doc sits on top of `Spending`'s; `find_by_ids` (`:880`) has
    none.
  - **Fix:** move those six lines onto `find_by_ids`.
  - **Size:** S. **Needs approval:** no.

- **`delete` documented as "Forgets"**
  - **Where:** `transaction_index.rs:346`, `wallet_index.rs:742`.
  - **What's wrong:** "forget" is a different act (`forget` deletes a pending transaction).
  - **Fix:** "Deletes index `id`".
  - **Size:** S. **Needs approval:** no.

## Schema (`init.sql`, `seed.sql`)

- **"facts" after 087** (top 9)
  - **Where:** `init.sql:14,19,23,220`, `seed.sql:2`, `models/block.rs:295`.
  - **Fix:** "transactions and legs".
  - **Size:** S. **Needs approval:** no.

- **`transaction_index.broadcasted_at` is a position** (top 9)
  - **Where:** `init.sql:244,262`; `transaction.broadcasted_at` (`init.sql:88`) is Unix ms.
  - **What's wrong:** one name, two units; `move_start` (`transaction_index.rs:273`) writes a start
    into it.
  - **Fix:** rename the column to `start` (or `from_position`), and `base::TransactionIndex`'s field
    with it.
  - **Size:** M. **Needs approval:** yes (base field).

- **Looked at, keep:** the other nullable columns are real absences (pending, genesis, Solana's
  missing `block_time`, watch-only keys).

## Usecase (`apps/server/src/usecase`)

- **Planned:** `usecase/indexes/{wallet,transaction}.rs` (see top). Not repeated.

- **Stores that orchestrate (same pattern)** (top 2)
  - **Where:** `usecase/sdk/indexes.rs:21-34` (`insert` writes `wallet` and `wallet_index`),
    `usecase/sdk/transaction_indexes.rs:83-101` (`settle` writes `transaction` and
    `transaction_index`), `:105-117` (`give_up`, goes with 089 #1).
  - **Fix:** bodies move to `usecase/indexes/{wallet,transaction}.rs`; the store calls one function.
  - **Size:** S. **Needs approval:** no.

- **Workers that orchestrate (same pattern)** (top 2)
  - **Where:** `workers/broadcast.rs:832` (`fn delete`: `transaction` and `transaction_index`),
    `:423-438` (`watch` reads `transaction_index`, `network_index`, `block`),
    `workers/trigger/mod.rs:156` (`fn save` writes `trigger`, `task`, `trigger_run`).
  - **Fix:** `usecase/indexes/transaction.rs::{delete_versions, find_versions}`;
    `usecase::triggers::save_round`.
  - **Size:** M. **Needs approval:** no.

- **Key restore inside a store**
  - **Where:** `usecase/sdk/wallets.rs:27-55`.
  - **What's wrong:** `find_all` restores each key and checks the address: business, not a port.
  - **Fix:** `usecase/wallets.rs::restore_keyed(db, factories)`; the store calls it.
  - **Size:** S. **Needs approval:** no.

- **Two ports named `Sqlite`, filed beside resources**
  - **Where:** `usecase/trigger.rs:16` (next to `triggers.rs` and `workers/trigger/`),
    `usecase/tasks.rs:24`.
  - **Fix:** `TriggerStore` and `TaskStore`, under `usecase/sdk/` with the other ports; update
    `usecase/mod.rs:6-10` and `models/mod.rs:5`.
  - **Size:** S. **Needs approval:** no.

- **Broadcast job flags** (top 6)
  - **Where:** `workers/broadcast.rs:116` (`Progress.sent: bool` beside `outcome`), `:241`
    (`Watched::Expired { left: bool }`, matched on `left: true` at `:783`),
    `usecase/jobs.rs:24` with `broadcast.rs:847` (`progress() -> (Option<String>, bool)`).
  - **Fix:** one `stage` enum (`Recorded | Sent | Ended(Outcome)`); two `Expired` variants;
    `progress()` returns `Option<&Progress>`. Keep serde compatible for queued jobs.
  - **Size:** M. **Needs approval:** no.

- **Vague worker names**
  - **Where:** `workers/broadcast.rs:342` (`index`), `:676` (`kept`).
  - **Fix:** `watch_index`, `find_transaction`.
  - **Size:** S. **Needs approval:** no.

- **Booleans that pick the query** (top 6)
  - **Where:** `usecase/wallets.rs:158` and `models/wallet.rs:570` (`set_tag(…, add: bool)`),
    `usecase/networks.rs:433` (`transactions_page(…, pending: bool, …)`).
  - **Fix:** `add_tag` / `delete_tag`; `pending_transactions_page` / `transactions_page`.
  - **Size:** S. **Needs approval:** no.

- **`set_index_state` branches on the state**
  - **Where:** `usecase/wallets.rs:243-248`.
  - **Fix:** `resume_index` and `pause_index`; the api picks one.
  - **Size:** S. **Needs approval:** no.

- **Nested `if let` / `else if let`**
  - **Where:** `usecase/wallets.rs:71-86`, `workers/index.rs:169-182`.
  - **Fix:** `match (new.secret, given)` with flat arms; let-else on the step.
  - **Size:** S. **Needs approval:** no.

- **Every endpoint read loads every network's nodes**
  - **Where:** `usecase/networks.rs:169,195,236,282,360` (`endpoint::db::find_all` then
    `.nodes(network)`); `:322` `connected` reloads every network to rebuild one transport.
  - **Fix:** `endpoint::db::find_by_network`; `reload_transport(network)`.
  - **Size:** S. **Needs approval:** no.

- **Missing service check, wrong doc**
  - **Where:** `usecase/networks.rs:392` (`block`, `block_transactions_page` skip
    `container.service()`); `api/network_blocks.rs:85` promises a 404.
  - **Fix:** check in both, as `blocks_page` does.
  - **Size:** S. **Needs approval:** no.

- **The same shapes built twice**
  - **Where:** `SendIntent` field by field at `usecase/wallets.rs:403` and
    `workers/trigger/actions.rs:113`; a third `Transfer` at `workers/trigger/actions.rs:26`.
  - **Fix:** `SendIntent::new(from, asset, to, amount, fee)`; rename the action's to `SendAction`.
  - **Size:** S. **Needs approval:** no.

- **Three clocks**
  - **Where:** `usecase/triggers.rs:231`, `usecase/stats.rs:132`, and
    `i64::try_from(tasks::now())` at `usecase/sdk/transaction_indexes.rs:159,196`,
    `workers/broadcast.rs:368`.
  - **Fix:** one `models::now_ms() -> i64` over `tasks::now()`.
  - **Size:** S. **Needs approval:** no.

- **Tuples for rows**
  - **Where:** `usecase/jobs.rs:23` (`(i64, Task, i64, i64)` from `task::db::find_page`/`find_by_id`),
    `transaction::db::count` (`(i64, i64, i64)`), `trigger::db::count` (`(i64, i64)`),
    `usecase/stats.rs:171` (`match status { 2 => … }`).
  - **Fix:** named row structs; `task::status_of` for the code.
  - **Size:** S. **Needs approval:** no.

- **Chain crates named in a usecase**
  - **Where:** `usecase/wallets.rs:428` (`within_range` matches `chain_ethereum`, `chain_solana`,
    `chain_bitcoin`); `main.rs` claims to be the only place that names them.
  - **Fix:** each chain's amount bound on its factory or `Info`.
  - **Size:** M. **Needs approval:** yes (`sdk/base`).

- **Dead outside tests**
  - **Where:** `usecase/transport.rs:183` (`Transport::replace`), `:465` (`Clients::highest`), both
    `allow(dead_code)` "082".
  - **Fix:** delete; the tests use `load` / `ranked`.
  - **Size:** S. **Needs approval:** no.

- **Stale module docs**
  - **Where:** `usecase/mod.rs:3-10` (no `blocks`, `indexes`), `workers/mod.rs:4` ("until mined at
    a final block or expired"; 088 has five endings), `workers/prune.rs:29` (a `pending` job that
    does not exist), `db.rs:4` ("knows no model … no queue", but registers `CONFLICTS`/`BLOCKS`).
  - **Fix:** reword each; or move `CONFLICTS`/`BLOCKS` to `models/task.rs`.
  - **Size:** S. **Needs approval:** no.

## API (`apps/server/src/api`)

- **List pages not `{items, next_cursor}`** (top 8)
  - **Where:** `api/jobs.rs:167`, `network_blocks.rs:36`, `network_transactions.rs:45`,
    `network_block_transactions.rs:36`, `triggers.rs:49,295`, `wallet_index_legs.rs:35`,
    `wallet_tags.rs:35`, `wallets.rs:144`, … (keys `jobs`, `blocks`, `legs`, `tags`, …).
  - **Fix:** one generic `Page<T> { items, next_cursor }`; the UI (`layout.tsx:85`,
    `wallet.tsx:146,167`) and tests (`support/api.rs:98`, `routes.rs`, `cases/paused.rs`) follow.
  - **Size:** M. **Needs approval:** no.

- **The same list boilerplate in every handler**
  - **Where:** 12 × `map_err(Error::invalid_query)` in `api/*.rs`, then `page()`, `limit`,
    `cursor(limit)`.
  - **Fix:** an `IndexParams` extractor whose rejection is `Error`; `Page<T>` builds its cursor.
  - **Size:** M. **Needs approval:** no.

- **Leg kind and transaction state sent as ints** (top 7)
  - **Where:** `models/leg.rs:84`, `models/transaction.rs:143`; compared as numbers in
    `ui/…/chain.tsx:297,299,312`, `wallet.tsx:413`, `tests/support/api.rs:99`, `tests/cases/*.rs`.
  - **Fix:** snake_case strings, as job kind and index state already are.
  - **Size:** M. **Needs approval:** no.

- **Two network shapes**
  - **Where:** `Listing`, `LegPage`, `NetworkEndpoints` use `network: {chain, name}`; `Wallet`,
    `Transaction`, `Block`, `Trigger*` use flat `chain` + `network`.
  - **Fix:** flat everywhere; `ui/src/money.ts:39 networkOf` goes.
  - **Size:** M. **Needs approval:** no.

- **Path structs repeated**
  - **Where:** `api/wallet_transactions.rs:29` (`WalletPath`, copy of `api/wallets.rs:181`),
    `network_blocks.rs:75` (`BlockPath`), `network_endpoints.rs:238` (`NodePath`) repeat
    `NetworkPath` and its `Network::new(Chain::new(..))`.
  - **Fix:** reuse `WalletPath`; `NetworkName` gets `into()` to `base::Network`.
  - **Size:** S. **Needs approval:** no.

- **A 409 patched by hand**
  - **Where:** `api/wallets.rs:123-131`.
  - **What's wrong:** rewrites `Conflict` to 409; every other conflict answers 422.
  - **Fix:** a `Kind::Duplicate` mapped to 409 in `api/mod.rs`.
  - **Size:** S. **Needs approval:** no.

- **"no facts kept"**
  - **Where:** `api/wallet_indexes.rs:116,138` (OpenAPI text; `ui/src/api/schema.d.ts:487,1521`).
  - **Fix:** "no legs kept"; regenerate `openapi.json` and `schema.d.ts`.
  - **Size:** S. **Needs approval:** no.

- **Checked, fine:** no DTO queries or saves; every function taking a `Page` ends in `_page`.

## Chains (`sdk/chains/{bitcoin,ethereum,solana}`)

- **`Info` client copied 3×** (top 10)
  - **Where:** `bitcoin/src/rpc_info.rs:15-97`, `ethereum/src/rpc_info.rs:13-83`,
    `solana/src/rpc_info.rs:15-86`; the `rpc::Client` forwarding impl again on each `Client`
    (`bitcoin/src/rpc.rs:299`, `ethereum/src/rpc.rs:529`, `solana/src/rpc.rs:432`).
  - **What's wrong:** same struct, `call`, forwarding and `decode`; only `tip`/`info` differ. Caused
    by `InfoClient: rpc::Client` (`base/src/info.rs:10`).
  - **Fix:** drop the supertrait (or one generic `Info<C>`); one `decode` in `packages/rpc`.
  - **Size:** M. **Needs approval:** yes (base).

- **`Client::tips` repeats `InfoClient::tip`**
  - **Where:** `bitcoin/src/rpc.rs:60`, `ethereum/src/rpc.rs:76`, `solana/src/rpc.rs:66`.
  - **Fix:** `tips()` calls the info client's `tip`.
  - **Size:** S. **Needs approval:** no.

- **`KeyPair` near-identical** (top 10)
  - **Where:** `ethereum/src/key.rs:8-85`, `solana/src/key.rs:8-85` (44 diff lines after renaming).
  - **Fix:** one generic `KeyPair<K, A>` in base.
  - **Size:** M. **Needs approval:** yes (base).

- **`Wallet` copied 3×** (top 10)
  - **Where:** `*/src/wallet.rs:94-139`: struct, `Addressed`/`Networked`, `transaction`,
    `from_pk`/`parse_address`.
  - **Fix:** a generic `base::KeyWallet<K, F>`.
  - **Size:** M. **Needs approval:** yes (base).

- **Builder plumbing copied 3×** (top 10)
  - **Where:** `bitcoin/src/builder.rs:324-354`, `ethereum/src/builder.rs:63-97`,
    `solana/src/builder.rs:59-96`: fields `default/payment/state/fee`, `with_default_source`,
    `with_fee`, the `from_transfer` frame.
  - **Fix:** a `base::Draft` with those four fields; each chain keeps its amount check.
  - **Size:** M. **Needs approval:** yes (base).

- **`Confirmer` same shape twice**
  - **Where:** `ethereum/src/chain.rs:121-146`, `solana/src/chain.rs:115-141`.
  - **Fix:** one base helper taking the `mined_in` closure.
  - **Size:** S. **Needs approval:** yes (base).

- **`block` / `blocks` defaults backwards**
  - **Where:** `base/src/block.rs:310` (`block` required), `:328` (`blocks` defaults to it); every
    chain writes `block` as `blocks(p, p).first()` (`ethereum/src/chain.rs:55`,
    `solana/src/chain.rs:68`, `bitcoin/src/chain.rs:64`).
  - **Fix:** `blocks` required, `block` the default.
  - **Size:** M. **Needs approval:** yes (base).

- **Bitcoin `confirmation_depth` vs `confirmations`**
  - **Where:** `bitcoin/src/config.rs:27,127,165,286`, `chain.rs:60` (±1 four times); `1..=1000`
    checked at `config.rs:124` and `chain.rs:27`.
  - **Fix:** store `confirmations` as the others do; validate once in `Config::validate`.
  - **Size:** S. **Needs approval:** no.

- **Small**
  - `"finalized"` literal 3× (`solana/src/rpc.rs:183,386,397`) → a `FINALIZED` const beside
    `CONFIRMED`. S, no.
  - One message for two checks (`solana/src/builder.rs:76,110`) → one message each. S, no.
  - Dead: `bitcoin::KeyPair::x_only` (`bitcoin/src/key.rs:73`),
    `bitcoin::Address::from_script_hex` (`bitcoin/src/address.rs:105`) → delete. S, no.

## Base (`sdk/base`) — every item needs approval

- **Two indexers, one body**
  - **Where:** `index/wallet.rs:318-371` vs `index/transaction.rs:430-475` (`next`/`close`/`skip`);
    store traits `wallet.rs:198-254` vs `transaction.rs:252-305` (`find_next`, `find_behind`,
    `move_start`, `set_state`).
  - **Fix:** one `IndexStore<I>` and one generic `Indexes`; each keeps only `close_one`.
  - **Size:** L. **Needs approval:** yes.

- **`settle` on both store traits**
  - **Where:** `index/wallet.rs:254`, `index/transaction.rs:305`.
  - **Fix:** `write_balance(index, holding)`, `write_confirmed(index, confirmed)`; server names
    (`wallet_index::db::settle`, `transaction_index::db::settle`) follow.
  - **Size:** S. **Needs approval:** yes.

- **`found` + `reverted` pairs**
  - **Where:** `transaction.rs:28,32` (`TransactionIndex`), `:207,210` (`Confirmed`);
    `mined_in`/`finalized_in` return `Option<(u64, bool)>` (`ethereum/src/rpc.rs:377`,
    `solana/src/rpc.rs:371`).
  - **Fix:** `found: Option<Found { position, reverted }>`.
  - **Size:** M. **Needs approval:** yes.

- **`Holdings::holds` returns `Vec<bool>`**
  - **Where:** `fee.rs:295`; spreads to `held: bool` in `ethereum/src/fee_estimator.rs:104`, a cache
    keyed `(Option<Address>, bool)` at `:46`, `solana/src/fee_estimator.rs:151-164`.
  - **Fix:** `enum Recipient { Holds, New }`.
  - **Size:** M. **Needs approval:** yes.

- **`with_settle(bool)`**
  - **Where:** `index/wallet.rs:82,101`; the server passes `balances.is_some()`
    (`usecase/wallets.rs:214`).
  - **Fix:** `with_balances(Option<Arc<dyn Balances>>)`.
  - **Size:** S. **Needs approval:** yes.

- **`Skipped { moved, stopped }` with an `and()` that ORs**
  - **Where:** `index/mod.rs:84-99`.
  - **Fix:** counts, or the moved/stopped indexes.
  - **Size:** S. **Needs approval:** yes.

- **`Birthday` means "start"** (top 9)
  - **Where:** `index/wallet.rs:18` (enum), `:40` (field), `:108` (`with_birthday`),
    `index/mod.rs:47`; ~100 references in Rust, SQL and TS, column `init.sql:233`.
  - **Fix:** `enum Start { Tip, At(u64) }`, field `start`, `with_start`; the server column and UI
    follow.
  - **Size:** M. **Needs approval:** yes.

- **"facts" in docs** (top 9)
  - **Where:** `block.rs:538,543,548,574`, `config.rs:15`.
  - **Fix:** "transactions, legs and rewards" / "fields".
  - **Size:** S. **Needs approval:** yes.

- **`Tips` holds one position**
  - **Where:** `block.rs:226`.
  - **Fix:** rename `Tip`.
  - **Size:** M (52 references). **Needs approval:** yes.

- **Dead and doubled paths**
  - `Address::belongs_to` (`address.rs:70`), `TransactionId::belongs_to` (`transaction.rs:157`): no
    callers → delete. S.
  - `pub mod container`, `pub mod transaction` plus re-exports (`lib.rs:10,21`): nobody uses the
    module paths → private `mod`. S.

- **Server's own `IndexState` copy**
  - **Where:** `apps/server/src/models/wallet_index.rs:81-100` mirrors `base::IndexState` for serde
    and `ToSchema`.
  - **Fix:** derive them in base behind the `openapi` feature; drop the copy.
  - **Size:** S. **Needs approval:** yes.

## Trigger (`sdk/trigger`)

- **`Index.keyed: bool`**
  - **Where:** `src/selection.rs:32`.
  - **Fix:** `enum Key { Kept, WatchOnly }`.
  - **Size:** S. **Needs approval:** no.

- **`CachedStore` memoizes twice by hand**
  - **Where:** `src/store.rs:46-70`.
  - **Fix:** one `cached(map, key, fetch)` helper.
  - **Size:** S. **Needs approval:** no.

## Packages (`packages/*`)

- **`Job::settle` is vague**
  - **Where:** `packages/tasks/src/job.rs:173`.
  - **Fix:** `after_ack` (callers `executor.rs:124`, tests `job.rs:253,277`).
  - **Size:** S. **Needs approval:** no.

- **`now()` repeats `millis()`; six-arg `blocks`**
  - **Where:** `packages/tasks/src/job.rs:161-168` vs `:218-221`; `:150` `blocks(id, status, run_at,
    attempts, job, now)`; `impl Job` split at `:78` and `:170`.
  - **Fix:** `now()` calls `millis`; `blocks` takes one `Waiting` struct; one `impl Job`.
  - **Size:** S. **Needs approval:** no.

- **`decimal` pub items with no outside caller**
  - **Where:** `packages/decimal/src/lib.rs:75` (`validate_amount`, used at `:89` only), `:159`
    (`checked_div`, tests only); crate doc `:20-21`.
  - **Fix:** `validate_amount` private; delete `checked_div`; fix the doc.
  - **Size:** S. **Needs approval:** no.

- **Stale doc**
  - **Where:** `packages/tasks/src/storage.rs:2` calls `MemoryStore` "the default store".
  - **Fix:** name the server's SQLite store.
  - **Size:** S. **Needs approval:** no.

- **`packages/crypto`:** checked, every pub item has a caller. Nothing to do.

## UI (`apps/server/ui/src`)

- **`money.ts` dead exports and wrong name**
  - **Where:** `money.ts:7` (`atomic`), `:17` (`format`, clashes with `_components/format.ts`),
    `:80` (`displayOf`): no importers; `:88` re-exports `shorten`.
  - **Fix:** delete the three and the re-export; rename the file `listings.ts`.
  - **Size:** S. **Needs approval:** no.

- **`${chain}/${network}` key written ~20 times**
  - **Where:** `networks/_components/add.tsx:26` (`name`), `networks/page.tsx:25` (`nameOf`),
    `nodes.tsx:62`, `rail.tsx:82,113`, `explorers.ts:16`, `layout.tsx:160`, `providers.ts:56`; the
    listings-to-networks set at `start.tsx:25`, `rail.tsx:328`, `jobs/page.tsx:119`, `money.ts:58`.
  - **Fix:** one `networkKey(n)` and one `networksOf(listings)`.
  - **Size:** S. **Needs approval:** no.

- **"facts" / "birthday" in the UI**
  - **Where:** `networks/_components/chain.tsx:344` (`Facts` shows hash and time),
    `jobs/page.tsx:289` (`facts`), `wallets/_components/watching.ts:4,37-38`.
  - **Fix:** `BlockLine`, `rows`; `birthday` follows the base rename.
  - **Size:** S. **Needs approval:** no.

## Tests (`apps/server/tests`)

- **Router built by hand nine times**
  - **Where:** `routes.rs:105-117`, and `369, 494, 601, 831, 878, 913, 1001, 1123`; the same in
    `support/mod.rs:81-114`; the `create` closure at `118` and `614`. Not in 089 #6.
  - **Fix:** one `support::router(db, container)`.
  - **Size:** S. **Needs approval:** no.

- **Nested `if let`**
  - **Where:** `tests/mod.rs:192-205` (`Build::signed`).
  - **Fix:** let-else, then `replacement_of(txid)`.
  - **Size:** S. **Needs approval:** no.

- **Stale module doc**
  - **Where:** `tests/mod.rs:1-5` (no `pipe`; "until it replaces them").
  - **Fix:** list the seven modules.
  - **Size:** S. **Needs approval:** no.

- **`storage.rs` is one 4 231-line file with stale names**
  - **Where:** `tests/storage.rs`; `:339` `…other_facts`, `:2457` `…with_their_facts`, `:166-171`
    `watch(birthday)`.
  - **Fix:** `storage/{block,leg,wallet_index,transaction_index}.rs`; "facts" → "legs and
    transactions".
  - **Size:** M. **Needs approval:** no.
