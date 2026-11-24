# Known issues (2026-11-23)

Found by a three-part QA run against the live server on 2026-11-23 (indexing and nodes; sends,
broadcast and triggers; API, jobs, UI and data), on the real database, testnets only, with the code
at 07338c0. It also covers the findings from testing on 2026-11-20, and re-checks every item of
the 2026-11-19 list (its ids are kept in brackets). Paths are under `apps/server/` unless they
start with `packages/` or `sdk/`.

Priorities:
- **P1:** wrong data;
- **P2:** money safety, availability, performance and operability;
- **P3:** hygiene.

A fixed item is removed.

## What works

- **Sends:** every send in the run landed `final` with no bumps:
  - 9 sends: ETH and USDC on sepolia, about 3 min each (12 confirmations); SOL on devnet, 20–27 s;
  - back-to-back sends from one wallet took consecutive nonces;
  - each job released its pending legs at the end; no transaction index was left open;
  - balances add up on both sides, including the trigger sweeps and fund-gas.
- **Data consistency:**
  - every balance view equals its last leg's total (22 indexes);
  - running totals check out; no orphan rows; no leg above `indexed_at`;
  - `foreign_key_check` gives 0;
  - the 178 adjustments left are all non-zero (10 883 zero ones were deleted on 2026-11-20).
- **API:**
  - every GET matches its spec schema; `/v1/stats` matches SQL;
  - cursor walks have no gaps or duplicates (except the jobs list, item 29);
  - bad decimals and zero amounts get a synchronous 400.
- **Indexing:** sepolia and devnet keep up (lag 0). Devnet's `getBlock` goes to the Ankr node only
  (endpoint 14), with 0 failed calls.
- **Other:** pause/resume, reconcile, every UI page, the UI type check, `schema.d.ts` up to date.

## By design

- **Ether sent by contracts [X4]:** internal transfers appear in no transaction field or log, so
  they are not indexed (`sdk/chains/ethereum/README.md`). Correct a balance with
  `POST /v1/wallets/{id}/indexes/{asset}/reconcile`.
- **A settle loses the history of the blocks it skips:** when an index falls below the line (an
  outage, a slow node), it settles: it reads the chain's balance at a final block and writes one
  adjustment for the difference. Movements in the skipped blocks show as that adjustment, not as
  transactions. The balance stays exact (checked on chain for devnet USDC `7YBM…`: every adjustment
  equals the on-chain change of its window). A tradeoff, not a fault.
- **A failed trigger payment [G1]:** a trigger acts at most once per `since` window; the broadcast job
  owns retries. A job that fails for good failed for a real reason, so the trigger waits for its
  next window.

## Summary

No P1 is open.

| # | P | one line |
|---|---|---|
| 3 | P2 | The block reader reads blocks no index needs (an optimisation) |
| 5 | P2 | The self-send funds check is too loose: spendable goes negative, a nonce can stick |
| 6 | P2 | Off-curve recipient, send to self and over-balance get 202, then fail [A3] |
| 7 | P2 | A token send over the balance fails `invalid` instead of `insufficient-funds` |
| 8 | P2 | The UI send dialog can wait forever [U3] |
| 9 | P2 | Trigger rounds never run on a network that always finds new blocks |
| 10 | P2 | Trigger definitions that cannot work are accepted [G4, F8] |
| 11 | P2 | Failed jobs are never pruned; a failing trigger keeps queuing them [G5] |
| 12 | P2 | Trigger rounds run on networks with no triggers [G6] |
| 13 | P2 | No logs: failures are invisible [X3, R6] |
| 14 | P2 | A 429 inside an HTTP 200 batch is not seen as a rate limit |
| 15 | P2 | Timeouts multiply: one index step hung 5 minutes [T1] |
| 16 | P2 | Throttle waits have no limit; a cancelled call keeps its slot |
| 17 | P2 | A node answering garbage is never demoted [D] |
| 18 | P2 | HTTP 500 with a JSON-RPC body loses the node's code [R4] |
| 19 | P2 | `-32005` too many results is treated as a rate limit [R5] |
| 20 | P2 | A pruned primary node moves an index's start [H2] |
| 21 | P2 | The networks list asks every node live, and the UI polls it every 4 s |
| 22 | P2 | Ethereum downloads full blocks and all receipts [I3] |
| 23 | P2 | Solana reads every slot [I4] |
| 24 | P2 | The network transactions list scans and sorts everything [I5] |
| 25 | P3 | A new index shows balance `0` instead of unknown [X6] |
| 26 | P3 | Settle overwrites `birthday` |
| 27 | P3 | 41 old adjustments are counted in received/sent |
| 28 | P3 | API error shapes [A5] |
| 29 | P3 | List API inconsistencies [A1] |
| 30 | P3 | UI lists read only their first page [U5] |
| 31 | P3 | Parallel index creates all answer 201 [A7] |
| 32 | P3 | Endpoint add calls any URL, timeout uncapped [A8] |
| 33 | P3 | Error messages lose the HTTP status and the node's message |
| 34 | P3 | Confusing names and stats docs |
| 35 | P3 | Full scans on jobs and stats [I6] |
| 36 | P3 | Leftovers and small inconsistencies [Q1] |
| 37 | P3 | Trigger round costs: no fee snapshot, BTC estimate's second connection [F10, F12] |
| 38 | P3 | One node refusing `eth_getBlockReceipts` switches the chain to per-transaction receipts |

---

## P2: money safety and stuck payments

### 5. The self-send funds check is too loose. P2
- **What:** two 0.03 ETH self-sends with 0.0484 spendable were both accepted;
  `balance_confirmed_spendable` read −0.0116 ETH for about 3 minutes, and every other send from
  that wallet was refused meanwhile.
- **Cause:** `save_pending` nets each index's legs (`models/transaction.rs:316-334`), so a self-send
  needs only its fee. The view subtracts the outgoing leg and ignores the incoming one
  (`init.sql:283-324`).
- **Risk:** on Ethereum a transaction needs value plus fee when it runs. If another spend takes the
  balance below that, the self-send sticks at its nonce and blocks every later nonce until a cancel
  lands.
- **Fix:** count only outgoing legs in the check, as the view does. Design:
  `docs/design/092-self-send-funds-check.md`.

### 6. Some send errors only show after a 202. P2 · [A3]
- **What:** the API accepts these, and the job fails moments later:
  - an off-curve Solana recipient: 2 ms;
  - a Solana send to self: 2 ms;
  - more than the balance: ETH 0.2 s, SOL 4.9 s.
- **Fix:** check the recipient (curve, not self) and the spendable balance before queueing; answer
  4xx.

### 7. A token send over the balance fails as `invalid`. P2
- **What:** job 166929 ended `invalid: …execution reverted: ERC20: transfer amount exceeds balance`,
  not `insufficient-funds`.
- **Cause:**
  - the gas estimate overrides only the native balance (`sdk/chains/ethereum/src/rpc.rs:239`),
    so the call reverts (`:265`);
  - `Stop::built` labels it `invalid` (`usecase/workers/broadcast.rs:208-214`);
  - so the funds check never runs, against design 088.
- **Fix:** check the token's spendable balance before estimating, or map a revert caused by a short
  token balance to `insufficient-funds`.

### 8. The UI send dialog can wait forever. P2 · [U3]
- **What:** `ui/src/pages/wallets/_components/send.tsx` stops reading the job after "accepted" and
  waits for a leg with the first txid. It never sees a bump, a replacement or an expired version.
- **Fix:** follow the job to its end state.

## P2: triggers

### 9. Trigger rounds never run on a network that always finds new blocks. P2
- **What:** a round is queued only when an index step finds no new block
  (`usecase/workers/index.rs:186`; `sdk/base/src/indexer.rs:286`). On devnet every step finds one,
  so the job re-runs at once without a round (`index.rs:194`).
- **Evidence:** zero `solana:devnet` rounds in 24 h, against 2 294 on solana:mainnet. No devnet
  trigger exists yet, so this was not seen failing a live trigger.
- **Fix:** queue a round after any step that moves the confirmed position, or on a timer.

### 10. Trigger definitions that cannot work are accepted. P2 · [G4, F8]
- **What:** a trigger with amount `-1.0000000000000000000001`, ids `["garbage"]` and `since` 1e17 s
  got 201. Values are checked only when a round runs, and a no-op is silent.
- **Fix:** validate the amounts, ids, networks and `since` at create and update; answer 4xx.

### 11. Failed jobs are never pruned. P2 · [G5]
- **What:** 30 failed broadcast jobs, the oldest from 1794624727658. The prune job deletes only
  `triggers` jobs (`usecase/workers/prune.rs:31`). A trigger that keeps failing is never held.
- **Fix:** prune failed jobs after a retention; hold a trigger after N failures in a row.

### 12. Trigger rounds run on networks with no triggers. P2 · [G6]
- **What:** in 10 minutes: 113 rounds on solana:mainnet and 16 on ethereum:mainnet, neither with a
  trigger. That's about 1.4k rows an hour on solana:mainnet.
- **Fix:** queue a round only when the network has an enabled trigger.

## P2: RPC nodes, timeouts and visibility

The unstaged 091 change covers part of this:
- tracing logs to stdout and the `log` table, with `network`, `node` and `job_id`;
- the `rpc.http` event per request (`Traced`);
- endpoint disable, automatic on 401, 403, 404 and the wrong chain (the usecase `Guard`);
- node statistics in `network_endpoint_stats` (`Counter`), state by newest `network_endpoint_failure` row.

Items 14–16 are still open in it.

### 13. No logs: failures are invisible. P2 · [X3, R6]
- **What:**
  - the server log stays empty;
  - the transport builds every node with an empty logger (`usecase/transport.rs:319`, "No sink
    yet");
  - no wallet index ever has a `reason`;
  - `/health` `failures` are lifetime totals (devnet 4918, sepolia 342);
  - `last_failure` keeps a code but not its message, e.g. `-32602` (a range beyond the head,
    which the code already handles).
- **Fix:** the logging change. Also keep the last error per network and index, and count recent
  failures.

### 14. A 429 inside an HTTP 200 batch is not seen as a rate limit. P2
- **What:** the public devnet node answers a `getBlock` batch with HTTP 200 and `{"code":429}` in 8
  of 12 entries. `Failure::is_rate_limited` knows only -32005 and -32003 (`packages/rpc/src/error.rs:240-247`,
  `http.rs:171-177`). So the node is not demoted, and the step fails on decoding.
- **Fix:** treat code 429 (and "too many requests") as a rate limit. The `rpc.http` event already shows
  it (`rpc_errors`).

### 15. Timeouts multiply. P2 · [T1]
- **What:** one devnet index step hung for the job's 5 minutes, with ~4 node calls and 0 failures.
  Likely cause:
  - with `rpc.max_batch=1`, a batch of 10 becomes 10 requests in a row, each with the whole 15 s
    (`packages/rpc/src/decorators/split.rs:50-53`, `client.rs:245-251`), so up to 150 s per node;
  - two fallback nodes reach 300 s;
  - `Counted` sits above `Split` and counts a batch only when it ends, hence "0 failed".
  - Also: one call is Retry × every node × 15 s, a send waits for every node
    (`usecase/transport.rs:268-282`), and the 5-minute limit covers a whole run, not one step
    (`usecase/workers/index.rs:40-44`).
- **Fix:** one deadline per batch, not per split call. The 60 s step deadline (in the unstaged 091
  change) caps the damage.

### 16. Throttle waits have no limit. P2
- **What:** `Throttle::take` (`packages/rpc/src/decorators/throttle.rs:75-85`) reserves a slot and
  sleeps without a cap; the HTTP timeout doesn't cover the wait. A cancelled call (e.g. a 3 s tip
  probe) keeps its slot, so later calls wait longer.
- **Fix:** give the slot back on cancel; fail as rate-limited when the wait would pass the timeout.

### 17. A node answering garbage is never demoted. P2 · [D]
- **What:** `observe` counts only retryable failures (`usecase/transport.rs:426-432`), so a node
  that answers undecodable data stays first, and the index job fails every run. The test case is
  ignored (`tests/cases/mod.rs:63`).
- **Fix:** demote on a decode failure too.

### 18. HTTP 500 with a JSON-RPC body loses the node's code. P2 · [R4]
- **What:** `packages/rpc/src/http.rs:274-277` drops the body of a non-2xx answer, so a send that
  the node refused with a JSON-RPC error reads "outcome unknown".
- **Fix:** parse the JSON-RPC error from a non-2xx body.

### 19. `-32005` too many results is treated as a rate limit. P2 · [R5]
- **What:** `packages/rpc/src/error.rs:240-243` treats every -32005 as a rate limit, so the node is
  demoted and the Ethereum log fetch never splits its range (`sdk/chains/ethereum/src/rpc.rs:467`).
- **Fix:** tell "too many results" from a rate limit by its message or data, and split the range.

### 20. A pruned primary node moves an index's start. P2 · [H2]
- **What:** when the first node no longer serves a block, an unstarted index moves its start up
  (`sdk/base/src/indexer.rs:209-227`), even if another node serves it. The test case is ignored
  (`tests/cases/mod.rs:60`).
- **Fix:** try the other nodes before skipping.

### 21. The networks list asks every node live. P2
- **What:**
  - `/v1/networks` asks each network's nodes for their tip on every call, 0.5–2 s
    (`usecase/networks.rs:105-160`);
  - the Networks page polls it every 4 s (`ui/src/pages/networks/page.tsx:22,72`);
  - the layout fetches it on every navigation (`ui/src/layout.tsx:154,175`);
  - this burns node quota and slows pages.
- **Fix:** a cheap list from stored state; live tips on demand or every ≥30 s.

## P2: performance

### 3. The block reader reads blocks no index needs. P2 · optimisation
- **What:** each step starts at the last kept block, clamped up to the line
  (`sdk/base/src/indexer.rs:255-268`; `next(line, through)` is capped at the last kept block).
  A settled index waits near the confirmed position, ~120 slots above the line on devnet. The
  reader spends its calls on the blocks in between, never reaches the index, the line passes it,
  and it settles again. With 64 confirmations on Ethereum it matters less; it is an
  optimisation, not a fault.
- **Evidence:** on 2026-11-20 devnet sat exactly 131 slots behind and wrote ~15 adjustments a
  minute until `getBlock` moved to a faster node.
- **Fix:** when no index needs a block below the lowest index's next position, start there.
  The reorg check must then accept a run that doesn't link to the last kept block. Base change
  (needs approval).

### 22. Ethereum downloads full blocks and all receipts. P2 · [I3]
- **What:** every non-empty block is fetched in full, with all its receipts
  (`sdk/chains/ethereum/src/rpc.rs:105-130, 607-620`), whether or not a watched address is in it.
- **Fix:** filter by logs and addresses first; fetch receipts only for matching transactions.

### 23. Solana reads every slot. P2 · [I4]
- **What:** one `getBlock` per slot, ~2.5–4 a second on devnet. Free nodes can't sustain it:
  - public devnet answers 429 on batches;
  - Tatum free 429s, and refuses `getBalance` and `getTokenAccountsByOwner` (-16401);
  - Helius' key is exhausted.
- **Fix:** a paid node (today Ankr carries it), or read by address (`getSignaturesForAddress` per
  wallet), which needs its own design.

### 24. The network transactions list scans everything. P2 · [I5]
- **What:** the query scans the whole table, sorts it in a temporary B-tree and aggregates legs
  per row: 55 ms for 23 403 sepolia transactions (debug build).
- **Fix:** an index matching the order, and the legs joined after paging.

## P3: hygiene

25. **A new index shows balance `0` instead of unknown [X6].**
    - The view turns "no leg" into `'0'` (`COALESCE(c.total,'0')`). It shows until the first
      settle, and on Ethereum while the settle's block is above `network_index.confirmed`.
    - Fix: null until the first leg at or below confirmed.
26. **Settle overwrites `birthday`.**
    - `settle` sets `birthday = position` (`models/wallet_index.rs:708-718`), so every index shows
      a birthday minutes old, while its legs go back to `requested_from`.
    - The UI then says "history from #… (asked #…)" (`ui/…/watching.ts:36-38`).
    - Fix: rename the column to what it is (facts start here), or show `requested_from`.
27. **41 old adjustments are counted in received/sent.**
    - On sepolia `0x9569…` and `0xf150…` native, and devnet `7YBM…` USDC, older adjustments moved
      `total_received`/`total_sent`. The current rule counts them in neither
      (`models/leg.rs:254-259`).
    - `balance_confirmed_received` is too high there; the balances are right.
    - Fix: a one-off recompute.
28. **API error shapes [A5].**
    - A bad path param gives 400 `text/plain`. Unknown routes give 404/405 with an empty body.
    - Every body error, even a 3 MB body, gives one generic 400: no 413, no 415, no field named.
    - A 5 000-char id is echoed back whole.
    - `blocks/{position}` on an unknown network says "no live block" instead of "network is not
      configured".
29. **List API inconsistencies [A1].**
    - Every list answers its own key (`wallets`, `jobs`, …), not `{items, next_cursor}`.
    - A full last page still returns a `next_cursor`, then an empty page (`db.rs:141-142`).
    - Wallets, triggers and tags accept any cursor (200 `[]`); the other lists answer 400.
    - The jobs list pages by `updated_at`, so a job updated between two reads is never listed
      (`models/task.rs:378-388`).
    - Unknown query params are ignored; `?kind=bogus` gives 200 `[]`, while `?state=bogus` gives
      400.
30. **UI lists read only their first page [U5].**
    - Jobs, triggers, runs, tags and the mempool list (`jobs/page.tsx:104`,
      `triggers/page.tsx:85,92`, `view.tsx:54`, `chain.tsx:77`).
    - The 10 s refresh replaces "Older" legs (`wallet.tsx:138-171`).
    - A slow answer for an old wallet or network can overwrite the new one.
    - A null balance shows "0" (`wallet.tsx:46,51`).
31. **Parallel index creates all answer 201 [A7].**
    - 8 at once all got 201 for one row (check then insert, `usecase/wallets.rs:195-205`).
    - Fix: insert-or-ignore, and answer whether it created.
32. **Endpoint add calls any URL [A8].**
    - There is no host check (loopback, private IPs) in `usecase/networks.rs` `verify`.
    - `rpc.timeout_ms` is uncapped (`packages/rpc/src/http.rs:69-71`).
33. **Error messages lose detail.**
    - "JSON-RPC endpoint rejected the request" drops the HTTP status (`packages/rpc/src/http.rs:274-277`;
      only the error kind keeps it).
    - Node counters keep the last code, not its message.
34. **Confusing names and stats docs.**
    - `transaction_index.broadcasted_at` holds a block position; `transaction.broadcasted_at`
      holds Unix ms (`init.sql:244`). A name like `broadcast_tip` would be clearer.
    - Stats `movements` counts adjustments too; `tags` counts tag uses (5), not distinct tags
      (3).
35. **Full scans on jobs and stats [I6].**
    - The jobs list scans `q` and sorts; the status count scans `q`; the leg count scans the
      whole `leg_block` index. It's fast while `q` is small (3 287 rows).
    - Fix: an index on `(status, updated_at, id)`; running counters.
36. **Leftovers and small inconsistencies [Q1].**
    - 31 old-shape broadcast jobs show `txid: null`.
    - Triggers jobs 7415, 7444 and 7458 fail on "missing field `selected`".
    - Finished jobs show `run_at` ~50 s after their last update.
    - Legs and runs of unknown ids give 200 `[]`.
    - `/health` sepolia `blocks` reads 59 673 against 12 341 rows.
    - Ethereum block hashes have no `0x`.
    - `money.ts`'s header still says display units, and `displayOf` is unused.
    - `nodes.tsx` shows no `rps`.
    - A trigger update answers the rebuilt row (`last_round` null; `usecase/triggers.rs:165-179`).
37. **Trigger round costs [F10, F12].**
    - The BTC fee estimate runs `block_on` inside the round's DB read
      (`usecase/workers/trigger/mod.rs:97-113`).
    - Each fee term calls the estimator, with no snapshot per round (`amounts.rs:137`).
    - Low impact today: no Bitcoin network, and rounds take ~3 ms.
38. **One node refusing `eth_getBlockReceipts` switches the chain to per-transaction receipts until
    restart** (`sdk/chains/ethereum/src/rpc.rs:409-424, 662-666`). Not live: sepolia has one node.

## Cleanup

Left in the database (the API has no wallet DELETE):
- `ethereum.sepolia.0x7e5f4552091a69125d5dfcb7b8c2659029395bdf`, imported with the publicly known
  private key `0x…01`: remove by hand;
- `ethereum.sepolia.0x571c8a721f854c3423f39fe07c20fa4b672c442b` (generated key);
- watch-only `ethereum.sepolia.0x000000000000000000000000000000000000dead` and
  `solana.devnet.11111111111111111111111111111112`;
- `qa-send-sol` (`57LVrNVWcXxpL3SBEgkdNCUxZ8pGQrbpJxj2CrhbyZsv`, empty), `qa-send-eth`
  (`0xa27b3cbc6c301edd684ad1266cc519978eb398c6`), `qa-index-devnet-mint` (watch-only).

Node changes made on 2026-11-20, through the API:
- Helius removed (key exhausted);
- public devnet throttled to `rpc.max_batch=1&rpc.rps=2`, Tatum to `rpc.rps=3`;
- Ankr added at `rpc.rps=10`, with `getBlock` routed to it only;
- devnet settings `{"read_depth":150,"batch":20}`.

DB change on 2026-11-23: Ethereum `confirmations` set from 12 to 64 on sepolia and mainnet, as in
`seed.sql` (takes effect at the next start).

Backups:
- `.data/payment.db.before-087-20261120-150717` (before the 085 and 087 migrations);
- `.data/payment.db.before-batch-20261120-153235`;
- `.data/payment.db.before-zero-adjustments-20261120-162757`;
- `.data/payment.db.before-confirmations-64-20261123-121110`.
