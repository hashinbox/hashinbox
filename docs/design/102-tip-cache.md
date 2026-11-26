# 102: One tip read per index step

Status: proposed (doc only). It touches `sdk/base`, which needs approval before it is built.

## Problem

On every index step, on every chain, the server asks for the chain's tip twice:

1. **Ranking** (`apps/server/src/usecase/nodes.rs`): before a call, the node layer reads each
   node's tip through its `base::CachedInfo` (`sdk/base/src/info.rs:40`), to pick the freshest
   node. That is one request per node.
2. **The indexer** (`sdk/base/src/indexer.rs:273-279`) then calls `self.chain.tips()` (the
   `Chained` trait). It goes through the chain's own RPC call, not through any node's
   `CachedInfo`, so it asks the chosen node again for what ranking learned milliseconds before.

`CachedInfo` doesn't help either, for a second reason: its TTL is one block time
(`CachedInfo::new(…, self.block_time)`, `nodes.rs:142`), and an idle step comes every ~3.7 s
on Tron (block time 3 s). So the cache has always expired by the next step, and ranking asks
every node again too.

## Seen live (2026-11-26, tron/mainnet, 136 min of `log`)

| | node 18, publicnode | node 19, TronGrid without a key |
|---|---|---|
| `node.rank` probes per min | 15.9 | 15.4 |
| `index.tips` reads per min | 12.9 | 3.2 |

- TronGrid answered 429 to about 1.5% of its requests (39 of 2,548). All 9 `index.tips` 429s came
  17–104 ms after a ranking probe to the same node: the probe-then-tip pair is what hits its limit.
- Each 429 on a real call demoted the node for 30 s, giving the "demoted, restored" log loop.
- No impact yet: tron/mainnet has no wallets, and the stored tip is 1 block behind the chain.
  It would matter with Tron wallets, because the heavy block reads would hit a rate-limited node.

## Change

1. **The indexer takes the tip ranking just read.** `chain.tips()` is answered from the chosen
   node's `CachedInfo` while that is fresh (`last()`, no request), and asks the node only when
   nothing fresh is known (first step, or after a pause).
   - `sdk/base`: the `Chained::tips` path, or the indexer loop taking a recent tip it is handed,
     whichever is smaller. The node layer hands down what it ranked with.
   - Blocks are read exactly as before. The confirmed line, finality and reorg handling are
     unchanged.
2. **`CachedInfo` outlives one step.** Its TTL becomes 2× the block time (`nodes.rs:142`), so one
   step's probe also serves the next step's ranking. Server only.

## Effect

- One tip request fewer per step on every chain (Bitcoin, Ethereum, BNB, Solana, Tron): about a
  third less tip traffic.
- TronGrid no longer gets the back-to-back probe-then-tip pair that drew most of its 429s.
- Risk: low. A tip one block stale is caught up on the next step, as it is today when a node lags.

## Related, config only (no code)

- Give TronGrid an API key (`?header.TRON-PRO-API-KEY=<key>` in the node URL; already passed
  through correctly), or drop keyless TronGrid from the mainnet seed (`apps/server/seed.sql`,
  `docs/design/100-migrate-1.sql`).
- Or throttle it: `&rpc.rps=1` in its URL (the per-node throttle exists,
  `packages/rpc/src/decorators/throttle.rs`).

## Smaller follow-ups

- A 429 on a ranking probe counts as a plain failure, not a rate limit (`nodes.rs:245-247`): pass
  the real flag.
- The "streak 3" in the demotion log is a fixed value, not a count (`nodes.rs:302-304`).

## Open questions

1. Approve the `sdk/base` change (part 1)?
2. TTL: 2× block time, or a fixed floor (e.g. at least 5 s) for fast chains like BNB (0.45 s)?
3. Keyless TronGrid: remove it from the seed, or keep it with `rpc.rps=1`?
