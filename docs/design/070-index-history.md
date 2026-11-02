# 070 — Index history: limits, start, state, pause

Status: agreed; being built.

## Semantics (unchanged core)

Indexing stays the simple model: per network one index job; each step picks
the earliest block any active wallet index still needs (`find_next`), else
re-reads the last kept block (reorg check), fetches a batch forward and stores
facts only for the indexes a block applies to. A network's kept blocks are
contiguous; a block in the middle is never skipped.

## 1. A limit on how old a start may be

- Each network's config takes `max_history_days` (optional; absent: no limit).
- `base::Service` carries it. Creating or watching a wallet with an explicit
  start older than `now − max_history_days` (resolved with
  `Dated::position_at`) is refused with 400 and the limit in the message.
  `"fresh"` is always accepted. "Fill by time" in the UI never offers earlier.

## 2. Start where the node can serve

- A wallet index keeps both `requested_from` (the start asked for) and
  `birthday` (the start actually indexed from; the API's `indexed_from`).
  Both equal the asked start at creation.
- When the block an index needs is not served by the node (pruned history:
  Ethereum JSON-RPC 4444, Solana "missing in long-term storage"/"cleaned up",
  Bitcoin "pruned data") and that index has **not started** (`indexed_at` is
  NULL), its `birthday` moves forward to the earliest block the node serves in
  full (binary search; pruning removes a prefix, so "served" flips once).
  `requested_from` stays, so the move is visible.
- An index that **has started** never moves its start.

## 3. A gap fails loudly

- When a block needed by an index that has started is not served (the server
  was down longer than the node keeps history), that index's state becomes
  `no_data` with the reason ("node no longer serves block N"), and the step
  fails with that error: the network shows it (red, `GET /v1/networks`), the
  index job's reason says it.
- A `no_data` index is skipped by later steps, so other wallets go on; if the
  network's own forward path is not served, every step fails, loudly, until an
  operator adds a node with the history.

## 4. State, stored only on events

`wallet_index.state`:

| state | set by | effect |
|---|---|---|
| `indexing` | creation, resume | normal |
| `paused` | `POST /v1/wallets/{id}/pause` | skipped by indexing (no facts kept for its blocks) |
| `no_data` | the indexer on a gap (3) | skipped; `reason` says why |

- Resume (`POST /v1/wallets/{id}/resume`) sets `indexing` (clearing `reason`)
  from `paused` or `no_data`; the index then continues from its `indexed_at`
  (it backfills what it missed while paused; no gap).
- Pause and resume act on every index of the wallet.

## 5. What the API shows per wallet index

| field | stored / computed | meaning |
|---|---|---|
| `index_state` | stored | `indexing` \| `paused` \| `no_data` |
| `index_reason` | stored | why `no_data`; null otherwise |
| `index_requested_from` | stored | the start asked for |
| `indexed_from` | stored | the start indexed from (≥ requested) |
| `indexed_at` | stored | the last block indexed; null before the first |
| `finality_at` | computed | the network's final (confirmed) block |
| `index_behind_tip` | computed | tip − indexed position, ≥ 0 |

All positions are native (height or slot). A client deduces the rest:
complete when `index_state = indexing` and `index_behind_tip = 0`; permanent
up to `min(indexed_at, finality_at)`; history shortened when
`indexed_from > index_requested_from`. Whether indexing is moving right now is
the network's condition (`GET /v1/networks`, its error), not stored per index.

## Schema

`wallet_index` gains `requested_from INTEGER NOT NULL`, `state INTEGER NOT
NULL` (0 indexing, 1 paused, 2 no_data) and `reason TEXT`. No migrations: a
fresh database.

## As built (deviations)

- A gap at block `p` (any index needing `p` or `p + 1`, the latter for the
  last kept block re-read on the forward path) finds the first served block
  `S` once (binary search over `[p, tip]`), then: every unstarted index with
  next block below `S` moves its birthday to `S`; every started one below
  `S` becomes `no_data` ("node no longer serves block <its next>"). Pruning
  removes a prefix, so everything below `S` is gone for all of them; handling
  them together avoids one failed step per index.
- Solana: `getBlocks` silently omits slots below the node's first kept block,
  so a range whose first slot is not listed also asks
  `getFirstAvailableBlock` (one extra call, only then); below it is
  `NotServed`. `-32009` stays "skipped"; `-32001` (cleaned up) is
  `NotServed`.
- `finality_at` and `index_behind_tip` come from the indexer's last step
  (`Stats::tip`, new `Stats::confirmed`): no node call per request; both are
  null before the network's first step.
