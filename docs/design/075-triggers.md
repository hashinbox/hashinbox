# 075: Triggers (pipes) — what they do, risks, test and bench plan

Status: investigation, no code changed. Paths under `apps/server/` unless noted.

## Decisions / findings first

1. **Rounds decide at the last kept block (the tip); the funds check uses `confirmed`.** `Triggers::new`
   takes `block::db::find_below(MAX)` (`src/usecase/trigger.rs:250`); every round balance is read there
   (`src/models/wallet_index.rs:731-740`). The broadcast check reads the view at `network_index.confirmed`
   (`src/models/leg.rs:416`, `init.sql:235-298`). Design 065/eval.md promised confirmed.
2. **No idempotency key.** A trigger is level-triggered; the only guard against sending twice is
   the sender's pending legs, written when the *broadcast job* runs, not when the round decides.
   Any second decision before that broadcast runs sends again (F1, F2 below). `Idle` is gone
   (`docs/design/068-index-job.md:64`).
3. **The gas-funding template double-funds.** `fund-gas` (`ui/src/pages/triggers/_components/forms.ts:428-459`)
   has `check: {all: []}`; pending *incoming* legs never count (`spendable` subtracts kinds 1,2,4
   only), so every round until the funding is mined sends the shortfall again.
4. **Silent no-ops.** A `select` target never answers "not fully indexed" (`wallet_index.rs:800`);
   a paused/behind/unindexed destination or source gives an empty set and `send` returns `Ok`
   with no job and no log (`src/usecase/workers/trigger/actions.rs:195-197`).
5. **Nothing above the unit level is tested.** `Round`, `Triggers::decide`, the index→round
   queueing, `/v1/triggers` and every failure path have no test (section 3).
6. **Cost is linear per trigger:** ~0.9 µs/network index + ~5.5 µs/picked index; 100k wallets = 0.62 s
   per select (section 5). Solana's 1 s interval makes a backlog (F1) reachable at ~10k wallets.
7. **Do:** dedupe rounds, one write per round, busy gate, then decide at `confirmed` (section 6).
   Deciding at `confirmed` alone makes finding 3 worse.

## 1. What triggers do today

**Lifecycle.** The index job, on `Next::Idle` (at the tip), enqueues a `triggers` job with a fresh
UUIDv7 id, no dedup (`src/usecase/workers/index.rs:127-136`), each interval (ETH 12 s, BTC 30 s,
Solana 1 s defaults). Topic `chain:network` conflicts with every broadcast of the network
(`chain:network.address`); the queue runs conflicting jobs in id order (`packages/tasks/src/jobs.rs:41-53`).
`Round::run` (`src/usecase/workers/trigger/mod.rs:82-152`): one `db.read` snapshot: read `last`, enabled
triggers with their `last_run`, decide every trigger in id order and run its action (pure: it only
builds jobs). Then per acting trigger, one write: `trigger_run` row + its broadcast jobs (`:130-145`).
`now` is the wall clock (`tasks::now()`), not block time. Any write error → `nack` → whole round reruns.

**Selection.** `Selector` → SQL (`src/models/wallet_index_selector.rs:64-90`): `ids`, `asset`, `tag`
(json_each EXISTS), `untagged`, `all`/`any`/`not`. Universe = the network's indexes indexed through
a live block at or above `last` (`wallet_index.rs:687-689`), so `not` is relative to it; paused,
no_data and backfilling indexes are out. Validation refuses empty `all`/`any`/`ids`, `not(not)`, bad tags.

**Filter / check / compare / aggregate.** Filter = `Logic<Compare>` run per index alone as `This`
(`trigger.rs:96-109`); each compare must read `This`. Check = `Logic<Since|Compare>` over the kept set.
`all` empty passes, `any` empty refused, evaluation in written order with short-circuit; errors stay
errors. Compare: `sum|min|max|count` of `of` vs `value`; `count` needs a whole `value`; unknown
(sibling not fully indexed, no asset) → `false`. `This{asset}` reads the same wallets' `asset` index
and is `None` unless every sibling is fully indexed (`wallet_index.rs:788-799`).

**Amounts** (atomic units of the send/compare asset, `trigger.rs:167-213`): `balance{of}` = Σ spendable
(balance at `last` − pending outgoing), `fee{of}` = estimator over `of`'s sources (0 for empty; not
cached — each use calls the estimator), `value` (display → atomic; too many decimals = error at
decide time), `add/sub/min/max`. Unknown term → whole amount unknown.

**`send` action** (`actions.rs:153-235`). Input `{source, asset, destination, amount}`; `This→This` refused
at create. Many sources → one destination (amount per source) or one source → many destinations
(amount per destination, `This` = that destination); many↔many refused at decide. Amount ≤ 0 or
unknown → that transfer skipped. Source not keyed or not in `asset` → refusal. One broadcast job per
sender, transfers in order (since 084: one broadcast job per payment, in walk order, all written in the
round's one write; each payment is checked on its own). Broadcast: sign, then in one write check `spendable@confirmed + takes ≥ 0`
per index and record pending (`src/models/transaction.rs:237-278`); short → job **failed
`insufficient-funds`**, nothing recorded (`src/usecase/workers/broadcast.rs:226`).

**Runs.** `trigger_run{id = trigger.now, started_at, jobs}`; written only when the action started ≥1
job, regardless of the jobs' later outcome; `triggers.last_run` points at it (`src/models/trigger_run.rs:73-83`).
`since` reads `last_run.started_at`. `GET /v1/triggers/{id}/runs` shows `started_at` only — not jobs,
not outcome (`src/api/triggers.rs:428-449`). Refusals are a `trigger.refused` warn log only (`mod.rs:113-127`).
`group_id`: one UUIDv7 per `POST` batch (1–16 triggers, one network, all-or-none); no route reads it.

**Idempotency.** Same change twice: yes when the decision repeats before the first broadcast
records (F1, F2, F3). After restart: `recover` puts the running round back to pending
(`src/models/task.rs:258`); it re-decides, and triggers whose run was already written decide again.
After a reorg: nothing is keyed to blocks; the next round re-decides on the new branch (F6, F7).
**Concurrency.** Round vs broadcasts of the network: serialized by topic. Round vs index job and
API: concurrent (WAL); the round's snapshot is its read; edits land after it.
**Failure.** Decide/action error → per-trigger log, others continue. Write error → round nack/retry.
Fee estimate (BTC) runs `find_unspent` on a *second* pool connection while the round holds one
(pool = 2..16 by CPU): two BTC-like rounds on a 2-CPU host wait on each other up to the pool timeout.

## 2. Risks: tip vs confirmed (each checked in code)

- **F1 Round backlog → duplicate sends.** A broadcast in retry (pending, earlier id) blocks every later
  round of its network; the index job keeps adding one round per interval. When it clears, R2 decides
  and mints jobs with ids *after* R3..Rn, so R3..Rn re-decide the same state: n−2 duplicate jobs per
  acting trigger. Sweeps (source = This, amount = all) end as `insufficient-funds`; fund-gas, fixed
  amounts and payouts from a rich wallet are **real duplicate payments**. Also reached by a round
  slower than the interval, or a manual API send stuck in retry.
- **F2 Partial round write → duplicates.** Runs are written one trigger per transaction; a failure on
  trigger k (e.g. trigger k deleted meanwhile → FK error on `trigger_run`) nacks the round; on retry
  triggers 1..k−1 decide again (their broadcasts have not run: blocked by the round's topic).
- **F3 Fund-gas re-fires while the funding is pending** (finding 3). Deciding at the tip it stops when
  the funding is mined; at `confirmed` it would repeat for `confirmations` more rounds.
- **F4 Sweep on unconfirmed funds.** Round sees tip balance → job → `insufficient-funds` failure.
  The run is still recorded. Without `since`: retried every round, ~one failed job per interval until
  confirmed (ETH ~12, Solana ~13 with 32 slots × 0.4 s / 1 s). It is not missed. **With `since`** the
  failed run consumes the window: the scheduled template retries 24 h later.
- **F5 Partial / starving amounts.** Amount = whole tip spendable; with any unconfirmed deposit on top
  of confirmed funds the send exceeds `confirmed` and fails; never a partial send. A deposit address
  receiving more often than every `confirmations` blocks **never sweeps**.
- **F6 Conditions on unconfirmed funds with another source.** Fund-gas, or any "when This receives,
  pay from treasury" fires on a tip deposit; the treasury's funds are confirmed so the broadcast
  passes. A reorg that drops the deposit leaves the payment (gas lost; payouts lost).
- **F7 Reorg after a sweep.** The broadcast spent only confirmed funds, so a reorg above `confirmed`
  cannot make it overspend; a reorg below `confirmed` is out of scope (expire handles dropped txs).
- **F8 Silent no-op** (finding 4) and **decide-time-only validation**: `value` precision, `count`
  with a non-whole or non-`value`, unknown `This{asset}` are accepted by the API and refused every round.

## 3. Tested today

Unit (`tests/pipe.rs`, in-crate, SQLite file, no workers): tags kept (174); selector leaves, combinators,
universe (200, 235, 277, 838); balances at an explicit position and pending sends (303, 329);
aggregates/operators (491); value precision (595); atomic amounts (611); siblings fully indexed (664);
logic order/errors and `since` before fee (690, 743); JSON form (758); storage of triggers/runs (792);
filter per index (855); send per source/destination, keyed, many↔many (887).
Model validation (`src/models/trigger.rs:558, 577, 606`). Balance guard incl. "the trigger read"
(`tests/storage.rs:1050`). **Not tested:** `Triggers::new` (which block), `Round`,
index→round queueing, runs written with jobs, refusal path, `/v1/triggers` (no case in `tests/routes.rs`
or `tests/cases/`), any timing/failure/concurrency path, F1–F8.

## 4. Deep suite scenarios (`tests/cases` style: API in, real workers, mock nodes, API out)

Controls today: chain `fund/mine/hold/reorg/finality_lag/prune`, node `lag/fork/prune/duplicate`,
faults `down/delay/fail_next/limit` with `Lost/Broken/Transport/Remote`. **Missing:** (M1) runs API
with jobs + outcome; (M2) trigger's last refusal via API; (M3) `App::restart` on the same file;
(M4) fail the Nth DB write; (M5) mock fee knob (gas price, BTC fee rate); (M6) server clock for `since`;
(M7) "rounds finished since" probe; (M8) bulk wallet+tag create. "Today" = now; "Want" = after section 6.

1. **[Selection]** Tag + asset picks only tagged wallets' indexes in that asset; untagged untouched. (M1)
2. `not tag` relative to universe: a paused index is in neither side. (M1)
3. Index added mid-life (backfilling) is not picked until at tip, then picked. (M7)
4. `ids` naming an index of another network/asset picks nothing; create still 201.
5. Tag added via API after create is picked next round; tag removed stops it.
6. **[Filters / compare / aggregate]** Filter `This sum above fee`: wallet at exactly fee is skipped, fee+1 atomic sweeps. (M5)
7. Check `count at_least 3` over kept set: fires at 3rd funded wallet only.
8. `min`/`max` over an empty kept set → no value → false; `sum` empty → 0.
9. `any`/`not` short-circuit: a failing fee RPC under a passed `any` does not refuse. (fail_next)
10. `This{asset: token}` with a wallet lacking a token index → compare false, no send.
11. **[Amounts / decimals / fees]** `balance − fee` on ETH: residual dust after sweep stays, trigger does not re-fire.
12. Token with 6 decimals, `value "0.0000001"`: Today create 201, refused each round (M2). Want 400.
13. Huge balance (2^200 atomic) round-trips exactly into the job's `amount`.
14. BTC many coins: fee grows with coins; amount = balance − fee is accepted by the node. (M5)
15. Fee estimate changes between filter and amount (fee TTL expiry) → amount ≤ 0 skipped, no job. (M5)
16. **[Send outcomes]** Sweep succeeds: one run, one job succeeded, legs mined once. (M1)
17. Two triggers drain one source in one round: second job `insufficient-funds`, visible. (M1)
18. Broadcast answer lost (`fail_next Lost`): same bytes, one spend, run counts one job.
19. Cluster: one node lagging refuses, other accepts (`lag`) → one spend.
20. Duplicate submit (`Duplicate::Reject`) → job succeeds, one spend.
21. Destination index paused: Today no job, no log (F8). Want refusal visible. (M2)
22. Source not keyed (watch-only) → refusal visible, no job. (M2)
23. **[Timing: unconfirmed → confirmed, reorg]** Deposit mined, not confirmed: Today ~1 failed job per round until confirmed (F4). Want no job until confirmed, then exactly one. (M1, M7)
24. Continuous deposits every block (< confirmations apart): Today never sweeps (F5). Want sweeps confirmed part.
25. Reorg drops a tip deposit before the round: no job.
26. Reorg drops the deposit after fund-gas fired: Today gas sent (F6). Want nothing sent before confirmed.
27. Reorg re-includes the deposit at another height: one sweep in total.
28. `since 2s` + unconfirmed deposit: Today the failed run blocks the window (F4). (M6)
29. **[Scale / concurrency / restart / idempotency]** Fund-gas with funding held in mempool (`hold true`) 3 rounds: Today 3 fundings (F3). Want 1. (M1)
30. Broadcast stuck in retry (`down` 5 intervals) then recovers: Today n−2 duplicate jobs (F1). Want none.
31. Fixed-amount trigger + F1: Today duplicate payments mined. Want one.
32. Delete trigger B while round writes A..C: Today round nack, A decides twice (F2). (M4)
33. Disable via `PUT enabled:false` mid-round: acts this round at most once, never after.
34. Restart between run writes: no duplicate after restart. (M3, M4)
35. Batch create with one bad trigger → 400, none stored; `group` same for all on success.
36. 1k wallets × 3 triggers: one round, all sweeps queued, round < interval. (M8)
37. `since 60` passes only after 60 s of wall clock; edits (PUT) keep `last_run`. (M6)

## 5. Performance (outside the test suite)

**Cost of a round.** Queries ≈ 2 + Σ_triggers [1 select + |kept|·(distinct `This{asset}` targets in
filter/amount) + distinct `select` targets in check/action] + 1 write tx per acting trigger.
Estimator calls ≈ Σ |kept|·(`fee` uses) (no per-round cache; BTC = 1 `find_unspent` each).
Select cost ≈ 0.9 µs × indexes of the network + 5.5 µs × picked indexes; per picked row: newest
leg seek, leg at `last` seek, pending-leg seek, one SUM_DECIMAL. Memory: every `Selection` holds full
`WalletIndex` rows (~1 KB each); `target` caches clones keyed by every id of `This` (80k ids per key).

**Plan** (`EXPLAIN QUERY PLAN`, no ANALYZE, as the server runs): `SCAN wi USING INDEX wallet_index_wallet`, `wallet`
joined twice, `leg_wallet_index` seeks for `n`, `c`, pending; view's `c` join elided; 1 id = PK search.

**Measured** (scratchpad generator: 1 network, 20k blocks, wallets × {native, token}, 10 mined legs
per index, 5% with a pending leg, 80% tagged `deposit`; Python sqlite3 3.51, warm, median of 3;
SUM_DECIMAL as a Python aggregate adds ~6% over a MAX stub; probe: session scratchpad `bench/gen.py`, `q.py`):

| wallets (indexes, legs, file) | tag+native | asset token | `not tag` | 1 id |
|---|---|---|---|---|
| 1k (2k, 20k, 16 MB) | 824 rows 3.6 ms | 1,000 rows 4.7 ms | 352 rows 1.6 ms | 0.011 ms |
| 10k (20k, 201k, 134 MB) | 7,990 rows 55 ms | 10,000 rows 57 ms | 4,020 rows 33 ms | 0.012 ms |
| 100k (200k, 2.0M, 1.3 GB) | 79,931 rows 619 ms | 100,000 rows 657 ms | 40,138 rows 398 ms | 0.012 ms |

Read: 100k deposit wallets with the fund-gas + sweep pair ≈ 2 × 0.65 s select + 2 × 100k × (sibling
read 12 µs + fee) ≈ 4–6 s per round in SQL alone; fine for ETH (12 s), over budget for Solana (1 s).

**Benchmark plan.** Where: an example binary `apps/server/examples/triggers_bench.rs` driving
`Triggers::new` + `decide` + `Round` writes over a generated file (needs a `lib.rs` split; the server is
bin-only — see Q6); never in `cargo test`. Generator sizes: wallets 1k/10k/100k; triggers 10/100/1000
(70% tag sweeps, 20% fund-gas pairs, 10% `select` checks); legs/index 10/100/1000; pending 0/5/50%;
networks 1/3. Metrics: round p50/p95, split select/filter/action/write, queries/round (diesel instrumentation),
rows, estimator calls (mock, counted), peak RSS, write-lock hold. Pass: round < 0.5 × interval.

## 6. Recommended changes (ranked)

1. **One pending round per network**: fixed job id `triggers.{chain}.{network}` (as the index job),
   enqueue only when none is pending/running. Kills F1 backlog duplicates. Small.
2. **One write per round** (all runs + jobs, all or none); a vanished trigger is skipped. Kills F2.
3. **Built-in busy gate**: a trigger does not decide while its last run has an unfinished job or a
   pending (unmined) transaction. Kills F3 and any repeat before settlement; restores 065's `Idle`.
4. **Decide at `network_index.confirmed`** (pass it as `last`; universe stays "indexed through ≥ it").
   Kills F4, F5, F6; consistent with the broadcast check. Only safe together with 3.
5. **Make no-ops visible**: `select` targets with `ids` missing → `None`; empty source/destination →
   refusal; store `last_error` on the trigger; runs API returns jobs and their states (M1, M2).
6. **Validate at create**: `value` precision vs asset, `count` whole `value`, asset ids configured.
7. **Per-round fee cache** keyed by target ids; BTC estimate on the round's connection. 8. **Log round
   cost** (ms, triggers, rows, jobs); warn when > interval.

## Open questions (recommended answers)

1. Decide at confirmed or tip? — Confirmed, with the busy gate (6.3) shipped first or together.
2. What ends "busy"? — Every job of the last run finished and none of its transactions pending.
3. Does a failed run count for `since`? — No: `since` reads the last run with a succeeded job.
4. Round dedup by fixed id? — Yes, same pattern as `index.{chain}.{network}`.
5. Empty destination/source: error or nothing? — Refusal (logged + on the trigger), never silent.
6. Bench home? — Add `src/lib.rs` re-export so `examples/` can link; until then the scratchpad SQL probe.
