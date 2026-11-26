# 097: Wallet events: what changed a balance, as a feed and a stream

Status: proposed (doc only). Open questions are at the end.

## Problem

- A third party reads a balance (`GET /v1/wallets/{id}`) but cannot see *why* it changed: what
  came in, what went out, what a reorg took back.
- It wants "every change since X", then each new change live, and must be able to check that
  the changes add up to the balance it reads.
- A browser "pay" SDK wants one wallet's changes live, with a narrow token (`096`, `packages/scope`).
- Logs (`091`) do not do this: they are pruned by count, published before commit (`id` 0) and
  drop entries when a listener lags.

## Rules

- One table, `wallet_event`: one row per thing that moved a wallet index's balances.
- An event is written **in the same write** as the change it tells (legs, pending rows,
  `network_index.confirmed`). A committed balance always has its events; a rolled-back write has
  none.
- History is never rewritten: a reorg or a dropped send writes a **compensating** event.
- Each event carries the index's balances **after** it, read from `wallet_index_with_balance`
  inside the same write.

## Table

```sql
-- One change to a wallet index's balances (design 097), written in the
-- write that made it. `id` is the cursor: the server has one writer
-- (`BEGIN IMMEDIATE`), so ids grow in commit order and a reader after id N
-- never misses a later commit. AUTOINCREMENT: never reused after a prune.
CREATE TABLE wallet_event (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    at              INTEGER NOT NULL, -- Unix ms, written.
    wallet_id       TEXT    NOT NULL,
    wallet_index_id TEXT    NOT NULL REFERENCES wallet_index (id) ON DELETE CASCADE,
    kind            TEXT    NOT NULL,
    -- What it is about: a transaction id, or a leg id (a reward or an
    -- adjustment).
    subject         TEXT    NOT NULL,
    data            TEXT    NOT NULL CHECK (json_valid(data)),
    CHECK (wallet_index_id LIKE wallet_id || '.%'),
    CHECK (kind IN ('pending', 'mined', 'confirmed', 'reorged', 'dropped', 'adjusted'))
);
-- A wallet's events, in order: the per-wallet list and stream.
CREATE INDEX wallet_event_wallet ON wallet_event (wallet_id, id);
-- `since=<time>`, and the prune.
CREATE INDEX wallet_event_at ON wallet_event (at);
-- Confirmed once: `confirmed` may step back on a failover and forward again.
CREATE UNIQUE INDEX wallet_event_confirmed ON wallet_event (wallet_index_id, subject)
    WHERE kind = 'confirmed';
```

- **Cursor = `id`**, not time: times collide within a write and clocks move. `since=<Unix ms>` is
  still accepted and maps to the first id with `at >= since`.
- `kind` is text: third parties read it as is, and it is the SSE `event:` name.
- Cascade with the wallet index (and so the wallet): a removed index has no feed.

## Granularity: one event per transaction per wallet index

| Option | Verdict |
|---|---|
| per leg | Too noisy: a UTXO send has many legs; a self-send has two that net to −fee. |
| **per (transaction, wallet index)** | **Chosen.** "Wallet x received N of asset A"; balances are per asset. |
| per wallet | Mixes assets: a token transfer moves the token index and the native index (fee). |

- A token transfer gives two events with the same `subject`: the token index (amount) and the
  native index (fee).
- A reward or an adjustment has no transaction: its `subject` is its leg id.

## Envelope (API shape)

Every event, in the list and the stream:

```json
{
  "id": 1042,
  "at": 1795600000123,
  "kind": "mined",
  "wallet_id": "ethereum.mainnet.0xabc",
  "wallet_index_id": "ethereum.mainnet.0xabc.eth",
  "subject": "ethereum.mainnet.0x9f…",
  "data": { "…": "per kind, below" }
}
```

### Change (in `data`, every kind)

- `txid`: null for a reward or adjustment.
- `amount`: net signed atomic decimal for this index (received − sent − fee).
- `received`, `sent`: unsigned parts; `sent` includes `fee`.
- `fee`: what this index paid in fee ('0' when none).
- `amount_usd`: as `leg.amount_usd` summed; null while pending or unpriced.
- `position`: block position; null while pending.
- `balance`: the snapshot, below.

### Balance snapshot (`data.balance`)

The index's `wallet_index_with_balance` row after the change, as it reads:

```json
"balance": {
  "confirmed": "1500000000000000000",
  "spendable": "1400000000000000000",
  "pending": "1450000000000000000",
  "confirmed_received": "3000000000000000000",
  "confirmed_sent": "1500000000000000000",
  "confirmed_received_usd": "9120.50",
  "confirmed_sent_usd": "4410.00",
  "finality_at": 21000059,
  "indexed_through": 21000123
}
```

- Per wallet index (per asset), never per wallet.
- Read once per touched index at the end of the write, so every event of one write for that
  index shows the same final state. The last event of a write is what `GET /v1/wallets/{id}`
  returns right after it.

## Kinds

| Kind | When | Written by (same write) |
|---|---|---|
| `pending` | Our send is recorded; or a send of ours to a wallet we also index (incoming) | `transaction_indexes::record` / `restore` (`save_pending`) |
| `mined` | Its legs land in a block above `finality_at` | `blocks::apply` |
| `confirmed` | Its block is at or below `finality_at` | `blocks::apply` (block already final), or the index worker's `network_index::db::save` |
| `reorged` | A reorg (or a reconcile) took its block back | `blocks::delete_from`, `wallets::reconcile` |
| `dropped` | A pending send was deleted without landing | `transaction_indexes::forget`, broadcast `delete` (`end`) |
| `adjusted` | A settle or a write-off adjustment | `sdk/indexes.rs` settle, `indexes/wallet::write_off` |

A transaction's life: `pending`? → `mined`? → `confirmed`, with `reorged` going back one step and
`dropped` ending a pending one. `pending` is skipped for a foreign transaction. `mined` is always
written, so Σ `amount` adds up (see Delivery).

### `pending`: our send (outgoing)

```json
{ "kind": "pending", "subject": "ethereum.mainnet.0x9f…",
  "data": { "txid": "0x9f…", "ours": true, "amount": "-30210000000000000",
    "received": "0", "sent": "30210000000000000", "fee": "210000000000000",
    "amount_usd": null, "position": null, "balance": { "…": "…" } } }
```

### `pending`: incoming

Only for a send of ours to another wallet this server indexes (`save_pending` keeps the
receiver's legs). Foreign mempool transactions are not watched today (open question 1).

```json
{ "kind": "pending", "subject": "ethereum.mainnet.0x9f…",
  "data": { "txid": "0x9f…", "ours": false, "amount": "30000000000000000",
    "received": "30000000000000000", "sent": "0", "fee": "0",
    "amount_usd": null, "position": null, "balance": { "…": "…" } } }
```

### `mined`

```json
{ "kind": "mined", "subject": "ethereum.mainnet.0x9f…",
  "data": { "txid": "0x9f…", "block": "ethereum.mainnet.0x77…", "state": "succeeded",
    "amount": "30000000000000000", "received": "30000000000000000", "sent": "0", "fee": "0",
    "amount_usd": "91.20", "position": 21000123, "balance": { "…": "…" } } }
```

- `state: "reverted"`: mined but failed on chain; `amount` is −fee. This is a **failed send that
  landed**: it goes on to `confirmed`, and its job ends `reverted` with no `dropped`.
- A reward: `txid` null, `subject` the reward leg id.

### `confirmed`

Same `data` as `mined`, with the snapshot after `finality_at` moved.

```json
{ "kind": "confirmed", "subject": "ethereum.mainnet.0x9f…",
  "data": { "txid": "0x9f…", "block": "ethereum.mainnet.0x77…", "state": "succeeded",
    "amount": "30000000000000000", "received": "30000000000000000", "sent": "0", "fee": "0",
    "amount_usd": "91.20", "position": 21000123, "balance": { "…": "…" } } }
```

### `reorged`

The compensating event: `amount` is what the `mined` added, negated.

```json
{ "kind": "reorged", "subject": "ethereum.mainnet.0x9f…",
  "data": { "txid": "0x9f…", "from": 21000120, "now": "gone", "reason": "reorg",
    "amount": "-30000000000000000", "received": "0", "sent": "0", "fee": "0",
    "amount_usd": "-91.20", "position": 21000123, "balance": { "…": "…" } } }
```

- `now: "gone"`: a foreign transaction, deleted; it comes back as `mined` if the new fork has it.
- `now: "pending"`: ours, back to pending. `amount` still negates the `mined` (the newest total
  loses it), and the snapshot shows `spendable` still held.
- `reason: "reconcile"`: `wallets::reconcile` deleted the legs above `confirmed` to read again.
- A reorged adjustment or reward: same shape, `subject` its leg id.

### `dropped`: a failed send that never landed

```json
{ "kind": "dropped", "subject": "ethereum.mainnet.0x9f…",
  "data": { "txid": "0x9f…", "reason": "expired", "replaced_by": null,
    "amount": "30210000000000000", "received": "0", "sent": "0", "fee": "0",
    "amount_usd": null, "position": null, "balance": { "…": "…" } } }
```

- `amount` gives back what `pending` held.
- `reason`: `refused`, `expired`, `cancelled`, `forgotten`, `jump` (broadcast outcome / `Why`),
  or `replaced` with `replaced_by` the new txid (a bump: `dropped` then `pending`, so the fee
  change is visible). A replaced version that lands anyway shows up as `mined`.

### `adjusted`

```json
{ "kind": "adjusted", "subject": "ethereum.mainnet.0xabc.eth.adjustment.21000000.0",
  "data": { "txid": null, "reason": "settle", "amount": "-120000000000000",
    "received": "0", "sent": "0", "fee": "0", "amount_usd": null,
    "position": 21000000, "balance": { "…": "…" } } }
```

- `reason`: `settle` (leg 0) or `write_off` (leg 1).
- Needed: an adjustment moves `confirmed` without a transaction; without it the events would not
  add up to the balance.
- Written at final positions, so it has no separate `confirmed`.

## Confirmed when finality moves

No leg is written when `finality_at` moves, so the index worker emits it:

- In the write of `network_index::db::save`, read the old `confirmed` (A) first, save, read the
  new (B).
- For B > A: the legs of the network's kept blocks with position in (A, B] (`block_position` then
  `leg_block`: only blocks that hold facts), one `confirmed` per (subject, index), snapshot after.
- `INSERT … ON CONFLICT DO NOTHING` on `wallet_event_confirmed`: B going back on a failover and
  forward again writes nothing twice.
- `blocks::apply` of a block at or below the current `confirmed` (catch-up, a new index's
  history) writes `mined` and `confirmed` in the same write.

## Reorgs

- `blocks::delete_from(X)` first reads the legs at or above X of the network (transactions,
  rewards, adjustments), grouped by (subject, index), then runs its seven steps, then writes one
  `reorged` per group with the snapshot after.
- Nothing is updated or deleted in `wallet_event`; a reader replays the feed and stays right.
- A reorg starts above `confirmed` (088), so a `confirmed` event is never undone.

## Endpoints

| Route | What | Scope |
|---|---|---|
| `GET /v1/wallets/{id}/events` | One wallet's events, oldest first | `com.hashinbox.wallets[{id}].events.$read` |
| `GET /v1/wallets/{id}/events/stream` | SSE, the same events live | `com.hashinbox.wallets[{id}].events.$read` |
| `GET /v1/events` | Every wallet's events, oldest first | `com.hashinbox.wallets.events.$read` |
| `GET /v1/events/stream` | SSE of the same | as above |

- Lists follow the convention: `IndexParams { cursor, limit }`, reply `{ events, next_cursor }`;
  model fns `wallet_event::db::find_page`, `find_by_wallet_page`. The cursor is an event id.
- `since=<Unix ms>`: start at that moment; ignored when `cursor` is given.
- Oldest first (a feed), unlike logs.
- `/v1/events` needs the unnarrowed scope (096: lists are not narrowed); a token of
  `wallets[a].events.$read` reads `/v1/wallets/a/events`.
- Scope schema gains `wallets.events => [read]`. `wallets[x].$read` does **not** cover
  `wallets[x].events.$read` (a longer path); a key needs `wallets.*.$read` or the events scope.
- Browser token: a JWT signed with `his`, `kid` = `hip`, `scope` =
  `com.hashinbox.wallets[ethereum.mainnet.0xabc].events.$read`, narrower than its key.

### The stream

- Each SSE message: `id: <event id>`, `event: <kind>`, `data: <the envelope>`.
- Start: `Last-Event-ID` header, else `?cursor=`, else `?since=`, else now.
- On connect it replays from the DB after the start id, then goes live.
- Live, through one in-process hub:
  - `Db::write` bumps a `watch` after every commit (one line in `db.rs`);
  - one hub task reads `id > last` once per bump, for all wallets, and publishes on a
    `broadcast`;
  - each stream filters by wallet and grant in memory.
  - So it is one query per commit, not one per listener.
- A lagging stream does not miss: on `Lagged` it reads the DB again after its last id, then goes
  back to the hub. There is no `missed` event, unlike logs.
- The stream ends at the token's `exp`. `EventSource` cannot set headers and retries with the same
  URL, so the pay SDK reconnects itself with a fresh token and `Last-Event-ID`. This needs `?token=`
  on GET (096 open question 4), or a fetch-based SSE reader.

## Delivery

- **At-least-once:** a client that reconnects with its last processed id may see an event again;
  it skips ids at or below its last.
- **Idempotency:** `id` is unique, never reused.
- **Order:** one total order by `id`, which is commit order, so a wallet's events are in causal
  order. Within one write: block position, transaction order, then index.
- **Resume:** keep the last id; list or stream from it. A cursor older than the oldest kept
  event gets `410 cursor-expired`. The client then reads balances (`GET /v1/wallets/{id}`) and
  streams from now.
- **Check:** for an index, Σ `amount` of `mined`, `reorged`, `adjusted` events equals the newest
  total. The last snapshot equals the API's balance.

## Retention

- The existing prune job deletes events older than `wallet_events_retention_days` (default 90),
  by `wallet_event_at`.
- Never by count: a feed with holes per wallet would lie; a hole is only ever "before the oldest".
- An index or wallet delete removes its events by cascade.

## Write-path cost

- Per kept block: one row per (transaction, index) with a leg in it, plus one
  `wallet_index_with_balance` read per touched index (about four index seeks). Blocks with no
  wallet legs: zero.
- Per finality step: one range read over the newly final kept blocks, plus one row per
  (transaction, index) in them. So each mined transaction costs about two events over its life
  (`mined`, `confirmed`).
- Per send: `pending` (sender, plus receiver if indexed), then `mined`/`confirmed` or `dropped`.
  A bump adds `dropped` + `pending`.
- A new index's history writes `mined` + `confirmed` per past transaction, once.

## Code placement

- `models/wallet_event.rs`: the model, `db::insert`, `find_page`, `find_by_wallet_page`,
  `find_after`, `prune`. Its own SQL only.
- `usecase/wallet_events.rs`: builds events from legs and snapshots. The usecases above call it
  inside their writes; it reads legs through `leg::db` and snapshots through `wallet_index::db`.
- `api/wallet_events.rs`: the four routes; the hub sits beside `usecase/logs.rs`'s feed in
  `AppState`.

## Migration

- `init.sql`: the table and its three indexes, after `wallet_index`.
- `docs/design/097-migrate-1.sql`: the same `CREATE`s for an existing file.
- There is no backfill: events start at migration. A client reads balances once, then follows the
  feed.

## Tests

In `apps/server/tests/` style, one test support module (086):

- An external receive: `mined` then `confirmed` when finality passes; each snapshot equals the
  view.
- Our send: `pending`, then `mined`, then `confirmed`; `spendable` drops at `pending`.
- A send to an indexed wallet: `pending` on both indexes (`ours` true/false).
- A self-send: one event per write, `amount` = −fee.
- A token send: two events (token index, native fee index), same subject.
- A reorg of a foreign transaction gives `reorged`, `now: gone`; of ours, `reorged`,
  `now: pending`; then `mined` again in the new fork.
- A reverted send: `mined` with `state: reverted`, `amount` −fee, no `dropped`.
- Refused or expired: `dropped` with that reason, and `spendable` back.
- A bump: `dropped replaced` with `replaced_by`, then `pending`.
- Settle and write-off: `adjusted`.
- `confirmed` goes back and forward again: no second `confirmed`.
- A block read already final: `mined` and `confirmed` in one write.
- A write that fails `insufficient-funds`: no event.
- The sum check: Σ amounts equals the newest total after a mixed run.
- API: the cursor pages oldest first; `since` works; a pruned cursor gets 410.
- SSE: `Last-Event-ID` replays the missed events; a lagging listener misses none.
- Scope: a `wallets[w1].events.$read` token gets 403 on w2, and `/v1/events` lists only w1.

## Open questions

1. **Foreign incoming pending:** watch the mempool, or node pending blocks, for transactions to
   our addresses? Today only transfers between our own wallets are seen before mining.
   Proposed: not in v1.
2. **Unconfirmed totals:** the view's received/sent are confirmed only. Add the newest mined
   leg's `total_received`/`total_sent` to the snapshot (a view change)? Proposed: yes, as
   `received`/`sent`.
3. **Backfill noise:** should a new index's history (blocks already final when read) be marked
   `backfill: true`, so a shop does not take it as new deposits?
4. **Retention:** 90 days is the default proposed here. Is that right, or should events be kept
   while the wallet exists?
5. **Index or wallet removed:** cascade (proposed), or keep the events and write a final
   `removed` event?
6. **`replaced`:** show a bump as `dropped` + `pending` (proposed), or as one `replaced` event?
7. **Confirmer-settled sends** (an index behind the line, `transaction::db::set_mined`, no block
   kept) write no mined legs, and their change reaches the balance through a later `adjusted`.
   Is a `confirmed` without legs needed for them?
8. **Global stream:** is `/v1/events/stream` wanted in v1, or only the list for server
   consumers?
