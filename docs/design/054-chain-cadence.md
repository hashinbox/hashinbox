# 054 — The chain declares its cadence

Status: proposal. Scope: `sdk/indexing` (`Schedules`, `Driver`, `Config`),
chain `IndexerParams`, `sdk/config::Cadence`. Amends 053: the rung-2 snippet's
`Driver::new(index, schedules, monitor)` becomes `Driver::new(index)`.

## Problem

The poll cadence is chain knowledge transported through config:

- Every chain section states `[cadence] poll = …`; the default is **one
  second for every chain** (`sdk/config/src/cadence.rs:38`). Solana and
  Bitcoin start from the same lie.
- `Schedules` (fallback + per-network map) exists only to ferry that number
  from the parsed document to the driver.
- The chain crate — which knows its own block time — is never asked.

The driver-side mechanics are fine and stay: `Schedule { interval, ceiling }`,
wait at tip, no wait while catching up, retry doubling capped at `ceiling`
(`sdk/indexing/src/sync.rs:464`).

## Decision

The chain declares its rhythm at indexer construction, next to its other
constants — exactly where it declares `Budget` and confirmed depth today:

```rust
// sdk/chains/bitcoin/src/indexer.rs
impl<C, R> IndexerParams<C, R> {
    pub const POLL: Schedule = Schedule::every(Duration::from_secs(10)); // ~half a block
    // ethereum: 6s, solana: 400ms — one constant per chain
}
// IndexerParams gains `schedule: Schedule` (defaults to POLL), flowing into
// indexing::Config { budget, schedule }.
```

- `Indexing` gains one method: `fn schedule(&self) -> Schedule`. The driver
  reads each unit's own rhythm from the unit it drives.
- `Schedules` (the map + fallback) is deleted. `Driver::new(index)`.
- The monitor has a default: the driver always runs the built-in `Report`
  and answers `health()` / `readiness()` itself. Readiness is what
  `payments.start()` waits on, so it is never optional plumbing.
- `.monitor(Arc<dyn Monitor>)` registers *additional* observers — a fan-out
  list, like listeners: metrics, logging. This is the override point. The
  `Monitor` trait stays because that substitution is real.
- Config keeps only an **override**: `cadence.poll` is `#[serde(default =
  chain's constant)]` — absent means the chain's rhythm, present means the
  operator adjusted it. Config stops inventing rhythms.
- `Composition.schedules` dies with `Schedules`; `payments.start()` wires
  nothing.

## Why not the alternatives

- `BlockInterpreter::block_time()`: interpreter's business is block content.
  Cadence is chain rhythm — a sibling of `ChainTips`, not of `inspect`.
- `ChainTips::block_time()`: the driver holds `Indexers<dyn Indexing>` and
  cannot reach the source; `ChainTips` methods are fallible RPC, this is a
  pure constant; and block-time → poll conversion is driver policy. The chain
  provides the rhythm it is willing to be polled at; the driver owns the
  backoff arithmetic around it.

## Changes

1. `sdk/indexing`: `Config` gains `schedule`; `Indexing::schedule()`; delete
   `Schedules`; `Driver::new(index)` with built-in `Report`, `.monitor(...)`
   for extra observers; health/readiness answered by the driver.
2. Chains: `IndexerParams::POLL` const + `schedule` field (default); passed
   into `indexer()`.
3. `sdk/config`: `Cadence` loses `poll` as a required value — becomes
   optional override with per-chain serde default; `Composition` loses the
   `schedules` field.
4. Update 053's rung-2 snippet and `payments.start()` description.

## Rollout

1. Indexing side (Config, `schedule()`, `Schedules` deletion, `Driver`).
   Gates.
2. Chain constants + params fields. Gates.
3. Config override + composition cleanup + 053/054 doc sync. Gates.
