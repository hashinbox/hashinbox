# Known issues (2026-11-04)

Paths under `apps/server/` unless they start with `packages/` or `sdk/`. Line numbers are from today's
tree. The other agent is editing `src/usecase/workers/{index.rs,trigger/mod.rs}` (F1/F2), so lines there may shift.
Priorities: **P0** money/safety · **P1** correctness/data loss · **P2** availability/perf · **P3** hygiene.
NEW = found while checking this list.

## Summary

| id | P | one line | status |
|---|---|---|---|
| F1 | P0 | Round backlog: later rounds re-decide, so the same payment goes out again | being fixed |
| F2 | P0 | One write per trigger: a failed write reruns the round and earlier triggers send again | being fixed |
| F3 | P0 | The fund-gas template sends the gas again every round until the funding is mined | open |
| F6 | P0 | A tip deposit triggers a payment from another wallet; a reorg removes the deposit, the payment stays | open |
| N1 | P1 | NEW Solana: every recent devnet/mainnet block fails `getBlock` (-32015, tx version 1), so indexing stops | open |
| N2 | P1 | NEW `balance_spendable` forgets a send once it is mined but not yet confirmed | open |
| E | P1 | `confirmed` is tip − N. No node's `finalized` limits it | open |
| B | P1 | Failover to a lagging node moves the tip and `confirmed` back, so the confirmed balance drops | open |
| A | P1 | A primary stuck on a dead fork stays primary for up to 120 s | open |
| C | P1 | One pruned node moves an index's start forward, and deposits a full node still serves are lost | open |
| I7 | P1 | Sepolia accepts starts up to 30 days back; the public node serves only ~33 h of receipts | open |
| F4 | P1 | Sweeps of unconfirmed funds fail every round, and a failed run still uses up the `since` window | open |
| F5 | P1 | A deposit address that receives often never sweeps | open |
| D | P2 | A node answering garbage is never failed over (744 failed steps in 5 s) | open |
| T1 | P2 | One 15 s timeout for everything. No deadline for a step or a send; boot can block | open |
| R1 | P2 | Short batch returns `Ok` | open |
| R2 | P2 | Duplicate batch id: a made-up empty failure (code 0) | open |
| R3 | P2 | An oversized request is called `Unavailable` (retryable), so it is retried for ever | open |
| R4 | P2 | HTTP 500 with a JSON-RPC body loses the node's code (send: "outcome unknown") | open |
| R5 | P2 | `-32005 more than 10000 results` counts as a rate limit; the range is not split | open |
| N3 | P2 | NEW `max fee per gas less than block base fee` is not seen as a refusal | open |
| I1 | P2 | `delete_in` scans every leg of an index for each block it saves | open |
| I3 | P2 | Ethereum downloads all receipts of every block (~0.5 MB per mainnet block) | open |
| I4 | P2 | Solana downloads whole blocks (~2 MB each, 2.5 per second) | open |
| I5 | P2 | The network transactions list scans and sorts all transactions | open |
| I8 | P2 | Send refusals are classified by message text, and the wording differs by client | open |
| F8 | P2 | Silent no-ops, and bad values are only refused when a round runs | open |
| F9 | P2 | The runs API shows `started_at` only: no jobs, no outcome | open |
| F10 | P2 | BTC fee estimate takes a second DB connection inside the round | open |
| F11 | P2 | Round cost at 100k wallets (4–6 s) is more than Solana's 1 s interval | open |
| F12 | P3 | Fee estimate is not fixed per round, so the TTL can expire mid-round or before broadcast | open |
| I2 | P3 | Idle networks never read their tip | open |
| I6 | P3 | The jobs list and stats scan whole tables | open |
| I9 | P3 | `eth_sendRawTransaction.underpriced.json` actually holds "insufficient funds" | open |
| R6 | P3 | `Statistics.last_failure` has no message | open |
| N4 | P3 | NEW `-32003` + "limit" anywhere in the message counts as a rate limit | open |
| N5 | P3 | NEW `packages/rpc/tests/http.rs` does not exist (empty `tests/` dir); the 3 transport findings have no test | open |
| A1 | P3 | Unknown query params are silently ignored on every paged list | open |
| A2 | P3 | Re-adding a wallet with a different `external_id` silently keeps the stored one | open |

---

## P0: money and safety

### F1: Round backlog sends duplicates. P0 · being fixed
- **What/when:** At the tip, the index job adds a `triggers` job every interval, with a new id and no dedup (`src/usecase/workers/index.rs:127-136`, `trigger/mod.rs:46-50`). A broadcast stuck in retry blocks the network's topic, so rounds pile up. When it clears, every queued round decides on the same state and sends again.
- **Evidence:** 075 §2 F1; scenarios 30, 31.
- **Fix (in progress):** a fixed job id `triggers.{chain}.{network}`, and a new round only when none is pending. Effort S.

### F2: Partial round write sends duplicates. P0 · being fixed
- **What/when:** Each acting trigger is written in its own transaction (`trigger/mod.rs:130-145`). If the k-th write fails (e.g. the trigger was deleted meanwhile, FK error), the round is nacked. The rerun decides triggers 1..k−1 again.
- **Fix (in progress):** one write per round; skip a vanished trigger. Effort S.

### F3: Gas funding is sent twice. P0 · open
- **What/when:** The template `fund-gas` (`ui/src/pages/triggers/_components/forms.ts:428-459`) has `check: {all: []}`. The gas it sends to the deposit is a *pending incoming* leg. `spendable` subtracts only outgoing kinds 1,2,4 (`src/models/wallet_index.rs:716-727`), so the shortfall still shows. Every round until the funding is mined sends it again. Deciding at `confirmed` would stretch this to N more blocks.
- **Options:** (a) busy gate (deep dive 2); (b) count pending incoming legs in the filter's view of `This`; (c) `since` on the template (it only hides the problem).
- **Recommend:** (a), plus (b) for this template. Effort M.

### F6: Payment on an unconfirmed deposit from another wallet. P0 · open
- **What/when:** "When This receives, pay from treasury" (or fund-gas) decides at the tip (`src/usecase/trigger.rs:250-251`). The treasury's funds are confirmed, so the broadcast check passes. If a reorg removes the deposit, the payment stays mined. The gas or payout is lost.
- **Options:** (a) decide at a final position (deep dive 1); (b) per-trigger "require confirmed This" flag.
- **Recommend:** (a), shipped together with the busy gate. Effort M.

---

## P1: correctness and data loss

### N1 (NEW): Solana indexing fails on every recent block. P1 · open
- **What/when:** `getBlock` is called with `"maxSupportedTransactionVersion": 0` (`sdk/chains/solana/src/rpc.rs:189-196`). Live probe today: 6 of 6 recent mainnet blocks and 4 of 4 devnet blocks answer `-32015 "Transaction version (1) is not supported…"`. The step fails at `decode` (`:210`) and fails again every interval. No Solana deposit is indexed. The same call with version 1 returned a 2 MB block with 398 transactions.
- **Options:** (a) raise to the highest version and check that the parser reads v1 messages; (b) on -32015, read the version from the message and retry once.
- **Recommend:** (a), and record a v1 block for the Solana mock. Effort S–M.

### N2 (NEW): `balance_spendable` forgets mined, unconfirmed sends. P1 · open
- **What/when:** spendable = balance at `confirmed` − *pending* outgoing legs (`position IS NULL`) (`init.sql:249-262`). When a send is mined, its pending legs are replaced by mined legs at the tip (`src/models/leg.rs:436-449`). For the next N blocks the spend is neither pending nor below `confirmed`, so spendable shows the money again. Example: 10 ETH, send 8 → spendable 2 → mined → spendable 10 for 12 blocks. The broadcast check (`src/models/transaction.rs:256-271`) then lets a second spend through. On account chains the node refuses it (failed job); BTC coin selection skips spent coins. The API shows the wrong number.
- **Options:** (a) subtract outgoing legs mined above `confirmed` as well (`position IS NULL OR position > confirmed`); (b) spendable = min(balance@confirmed, balance@tip) − pending out.
- **Recommend:** (a). It is one predicate in the view and in `wallet_index.rs:724`. It must land before "decide at confirmed". Effort S.

### E: `confirmed` ignores what nodes finalized. P1 · open
- **What/when:** `NetworkIndex::step` writes `confirmed = tip − confirmations` (`src/models/network_index.rs:47`). It drops `Stats.confirmed`, which the chain already reads: ETH `finalized` tag (`sdk/chains/ethereum/src/rpc.rs:142-159`), SOL `finalized` slot (`sdk/chains/solana/src/rpc.rs:141-142`), BTC tip − (depth − 1). If nodes stop finalizing (ETH non-finality, as in May 2023), deposits still count as confirmed after 12 blocks.
- **Evidence:** `tests/cases/finalized.rs` (ignored).
- **Options:** (a) `confirmed = min(tip − N, reported final)`; (b) a per-network `finality: depth | safe | finalized` setting; (c) keep depth and only warn when final lags.
- **Recommend:** (a). It costs ETH ~13–19 min instead of 2.4 min (deep dive 1); (b) can make that a choice later. The reported final comes from the primary only, so the "safest node" needs a min over all probed nodes. Effort M.

### B: Failover to a lagging node moves the tip back. P1 · open
- **What/when:** The cluster may fail over to a node up to `max_lag = 3` blocks behind (`packages/rpc/src/cluster.rs:45-56, 359-371`). The next step writes that node's tip (`sdk/base/src/indexer.rs:252-258`), so `tip` and `confirmed` go back. The confirmed balance of a deposit in the gap drops (test `ahead`). Triggers and the spend guard read the lower value.
- **Options:** (a) never let `tip`/`confirmed` decrease in `network_index` (keep the max; a real reorg archives blocks anyway); (b) fail over only to nodes at or above the last tip read; (c) `max_lag = 0`.
- **Recommend:** (a). Effort S.

### A: Primary stuck on a dead fork. P1 · open
- **What/when:** A node on a side branch answers without errors, so it is never demoted. Reselection runs only on failures, rate limits, or the 120 s refresh (`cluster.rs:219-228, 234-245`). Until then its block and leg stay live (test `stale`). With a fork deeper than N the confirmed balance is wrong.
- **Options:** (a) re-probe when another node's tip is ahead by more than `max_lag` (the probes are cheap); (b) refresh every few intervals (e.g. 15 s); (c) check the parent hash against a second node.
- **Recommend:** (b) now, (a) later. Effort S.

### C: One pruned node loses deposits. P1 · open
- **What/when:** The primary returns `null` receipts for a finalized block. That becomes `NotServed` (`sdk/chains/ethereum/src/rpc.rs:207-210`). `skip` then binary-searches only that node (`sdk/base/src/indexer.rs:204-238`). An index that has not started moves its birthday up (deposits below are lost). One that has started becomes `no_data` and stops. A `null` is a valid answer, so the cluster never fails over (test `pruned_one`).
- **Options:** (a) treat `NotServed` as a node fault: ask the other nodes before skipping; (b) a per-node "history floor" learned once, used for routing.
- **Recommend:** (a). Effort M.

### I7: Sepolia history limit vs public node. P1 · open
- **What/when:** `seed.sql` sets Sepolia `max_history_days = 30`. A live probe of `ethereum-sepolia-rpc.publicnode.com` (reth 2.8.0) serves receipts 9 000 blocks back and `null` at 10 800. That is ~33 h, which fits reth `--full`'s 10 064-block receipts window ([reth pruning](https://reth.rs/run/faq/pruning)). A start 2–30 days back is accepted (`src/api/wallet_indexes.rs:55-81`), then silently moved forward (C). Deposits in between are lost; only `birthday ≠ requested_from` shows it.
- **Options:** (a) set `max_history_days = 1` for Sepolia/mainnet on public nodes; (b) refuse at create when the node no longer serves the start (probe one block); (c) document "archive node needed".
- **Recommend:** (a) + (b). Effort S.

### F4: Sweep on unconfirmed funds; `since` window. P1 · open
- **What/when:** The round sees the tip balance, but the broadcast check uses `confirmed`. The job fails `insufficient-funds` (`src/usecase/workers/broadcast.rs:226`), and the run is still recorded (`trigger/mod.rs:130-145`). Without `since`: one failed job per round until confirmed (~12 on ETH). With `since 86400`, the failed run uses the window and the sweep waits 24 h (`src/usecase/trigger.rs:116-121`).
- **Options:** (a) decide at confirmed (deep dive 1); (b) `since` reads the last run with a succeeded job; (c) the busy gate.
- **Recommend:** (a)+(b)+(c) together. Effort M.

### F5: Busy deposit address never sweeps. P1 · open
- **What/when:** The amount is the whole tip spendable. Any unconfirmed deposit on top makes the send bigger than `confirmed` allows, so it fails every time. An address that gets a deposit at least every N blocks never sweeps. The send is never partial.
- **Recommend:** decide at confirmed. Then the amount is the confirmed part and the rest sweeps next time. Effort (with F4).

---

## P2: availability and performance

### D: Garbage answers never fail over. P2 · open
- **What/when:** A result that decodes as nothing asked is still a JSON-RPC success for `Http`/`Cluster` (`packages/rpc/src/testing/faults.rs:149,167`). `observe` only counts transport failures (`cluster.rs:234-245`). The decode error happens above the cluster (`sdk/chains/ethereum/src/rpc.rs:689-697`). The case run showed 744 failed steps in 5 s at the test's 5 ms interval: one failed step per interval for ever.
- **Options:** (a) the chain reports "bad answer" to the client (`Client::fault(node)`), and the cluster counts it as a transport failure; (b) decode inside `Http` against a schema (heavy); (c) the index job backs off exponentially on repeated equal errors.
- **Recommend:** (a), plus (c) as a storm guard. Effort M.

### T1: RPC timeouts and deadlines (user question). P2 · open
- **Today:** One `timeout_ms` per HTTP execution, default 15 s (`packages/rpc/src/endpoint.rs:47-68`, applied in `http.rs:53`). It is the same for every method and every batch: a 2-call `tips` batch and a 10-block receipts batch (~5 MB on mainnet, measured) get the same 15 s. Each chain wraps `Retry::new(Cluster::new(…))` (`sdk/chains/ethereum/src/config.rs:159-162`, same for SOL/BTC). Retry: 3 attempts, 250 ms then 500 ms backoff (cap 2 s) (`retry.rs:35-43, 96-106`). On a due reselection, a probe of all nodes runs inside the call (`cluster.rs:219-228, 249-265`), with up to 15 s more per attempt.
- **One logical call, worst case:** 3 × 15 + 0.25 + 0.5 = **45.75 s**. With a re-probe in each attempt: **~90.75 s**.
- **One ETH indexer step:** at least 4 logical calls in sequence: `tips` batch, blocks batch, receipts batch, `eth_getLogs` (`rpc.rs:173-220`). Log splitting adds calls (up to 19 for 10 blocks), and `null` receipts add a second `tips`. A step that ends in success after 2 timeouts per call: 4 × 45.75 ≈ **3 min**. With the default `batch` 100 instead of seed's 10, mainnet is ~30 MB of blocks + ~50 MB of receipts per step (measured ~0.3 MB + 0.5 MB per block), close to the 64 MiB response cap. `ResponseTooLarge` is not retryable, so the step would fail every interval. An index job runs up to `STEPS = 10` steps (`index.rs:35, 111`). No deadline wraps a step or a job (`usecase/workers/executor.rs` has none; `tasks` deadline means "not started after", `packages/tasks/src/rules.rs:47-50`). `skip` adds ~log2(range) full block reads.
- **Send:** It has no separate timeout. `eth_sendRawTransaction` is `Request::once`, so Retry does not repeat it (`retry.rs:136-138`). The cluster's fan-out waits for **every** node (`cluster.rs:188-216`), so one hanging node adds 15 s to every send. A refusal then calls `known()` (retryable, ≤45.75 s) (`sdk/chains/ethereum/src/rpc.rs:368-408`). Worst case ≈ 15 + 45.75 s, plus nonce/fee reads.
- **Boot:** `Cluster::new` probes before returning (`cluster.rs:145`). Networks open one after another (`sdk/payments/src/payments.rs:327-345`), so a black-holed node costs 15 s per network before the server serves.
- **Options:** (a) per-method bounds: probe/`tips`/`chainId` 3 s; single reads 5–10 s; batches `5 s + 1 s × calls`; send 10 s per node; (b) a total budget per logical call (e.g. 20 s across attempts) instead of attempts × timeout; (c) a step deadline: `timeout(max(2 × interval, 30 s), indexer.next())` → the job's reason is "step deadline"; (d) fan-out returns on the first acceptance after a short grace (e.g. 2 s) and lets the rest finish in the background; (e) probe at boot in the background (first node primary until done).
- **Recommend:** (c) + (b) first (small, bounds everything), then (a) for probes and batches, then (e). Keep send's per-node bound at 10 s with (d). Effort M.

### R1: Short batch returns `Ok`. P2 · open
- jsonrpsee sizes the answer vector by the number of responses received (`jsonrpsee-http-client-0.24.11/src/client.rs:436-442`). `Http::execute_batch` passes it on (`packages/rpc/src/http.rs:116-141`). ETH and SOL check the length (`sdk/chains/ethereum/src/rpc.rs:525-529`, `sdk/chains/solana/src/rpc.rs:388`). BTC `getblockhash` does not (`sdk/chains/bitcoin/src/rpc.rs:228-238`): a short answer looks like "chain ends here".
- **Fix:** check `results.len() == calls.len()` in `Http` → `InvalidResponse`. Effort S.

### R2: Duplicate batch id makes up a failure. P2 · open
- jsonrpsee fills the slots with `ErrorObject(0, "")` and overwrites them by id (`client.rs:440-466`). With a duplicated id, one slot keeps code 0, message `""`. It reaches callers as a node error object, and the node is never faulted.
- **Fix:** in `Http`, a code-0 empty failure or a repeated id → `InvalidResponse` (transport, counted against the node). Effort S.

### R3: Oversized request is retried for ever. P2 · open
- `transport::Error::RequestTooLarge` falls into `_ => Unavailable` (`http.rs:216`), which is retryable (`error.rs:47-56`). Retry repeats it 3 times, the cluster counts the node as failing and probes, and the index job repeats it every interval.
- **Fix:** map it to `InvalidRequest` (not retryable) and shrink the batch. Effort S.

### R4: HTTP 500 with a JSON-RPC body loses the code. P2 · open
- jsonrpsee drops the body on non-2xx (`transport.rs:334-337`), so it becomes `HttpStatus(500)` (`http.rs:204-207`). For `eth_sendRawTransaction` that is a transport error. `send` then says "outcome unknown" (`sdk/chains/ethereum/src/rpc.rs:377-381`) and the job is nacked and repeated, although the node said "nonce too low".
- **Fix:** a custom transport layer that parses a JSON-RPC body on 4xx/5xx; or a raw `reqwest` for broadcasts. Effort M.

### R5: `-32005 more than 10000 results` is not split. P2 · open
- `is_rate_limited` treats every `-32005` as a limit (`packages/rpc/src/value.rs:84-90`). `Http` turns it into `RateLimited` (`http.rs:97`). `transfers` only splits on error objects or `ResponseTooLarge` (`sdk/chains/ethereum/src/rpc.rs:478-489`), so the step fails, the primary is demoted, and the same range fails again. Infura answers both "more than 10000 results" and "query timeout exceeded" with -32005 ([Infura eth_getLogs](https://docs.metamask.io/services/reference/base/json-rpc-methods/eth_getlogs)).
- **Fix:** `Failure::is_too_much()` (message has "more than", "range", "too many", "size") checked before `is_rate_limited`; splitting treats `RateLimited` with such text as "split". Effort S.

### N3 (NEW): fee-cap refusal is "outcome unknown". P2 · open
- `reason()` (`sdk/chains/ethereum/src/rpc.rs:700-712`) matches "underpriced", "fee too low", nonce, funds and signature. geth and reth both say `max fee per gas less than block base fee` (geth `core/error.go`; reth `InvalidTransactionError::FeeCapTooLow`). `intrinsic gas too low` is missed too. These fall to "unknown", so the job is nacked and retried, and the funds stay held until `pending_ttl` (1 h).
- **Fix:** add the strings, with a corpus test over recordings. Effort S.

### I1: `delete_in` scans every leg of an index. P2 · open
- `DELETE … WHERE block = ? AND wallet_index_id IN (…)` (`src/models/leg.rs:401-412`) has no index on `block`. The plan is `SEARCH leg USING INDEX leg_seq (wallet_index_id=?)`, so each saved block reads every leg of each index it touches (`src/models/block.rs:273-277`). A hot deposit address with 1M legs means 1M rows per block.
- **Fix:** add `position = ?` (the block's) so `leg_wallet_index (wallet_index_id, position, seq)` seeks. Effort S.

### I3: Ethereum downloads all receipts. P2 · open
- `blocks()` asks for every non-empty block's receipts (`sdk/chains/ethereum/src/rpc.rs:201, 618-631`) only to get the status/fee of a few watched transactions. Measured on mainnet: ~0.5 MB receipts + ~0.3 MB full block per block.
- **Options:** (a) pass watched addresses to `blocks()`: only `eth_getTransactionReceipt` for transactions from/to them (step 2 of the indexing plan); (b) keep block receipts as a fallback when many match.
- **Recommend:** (a). Effort M (signature change in `base::Chained`).

### I4: Solana downloads whole blocks. P2 · open
- `getBlock` `transactionDetails: full` (`sdk/chains/solana/src/rpc.rs:184-196`), ~2 MB per mainnet slot (measured), 2.5 slots/s.
- **Options:** (a) follow addresses with `getSignaturesForAddress` (+ `getTransaction`), with slots as the cursor; (b) `transactionDetails: accounts`. Note: (a) changes reorg handling (signatures are per address, not per slot).
- **Recommend:** (a) for watched sets under ~10k addresses. Effort L.

### I5: Network transactions list scans everything. P2 · open
- `find_page` (`src/models/transaction.rs:362-402`): the plan is `SCAN t` + `TEMP B-TREE FOR ORDER BY`, with the correlated legs-JSON subquery per row. There is no index on `transaction(chain, network)` (`init.sql:67-84`).
- **Fix:** an index `(chain, network, block)` and keyset ordering on stored columns, or page the ids first and then join the legs. Effort S–M.

### I8: Send refusals by message text. P2 · open
- `reason()` uses substrings; codes differ by client. publicnode Sepolia (reth) sends `-32003 "insufficient funds for gas * price + value: have 0 want 714001"` (`sdk/chains/ethereum/src/testing/recordings/eth_sendRawTransaction.funds.json`). geth sends the same prefix with `address … have … want …`, code -32000. The publicnode recorder notes the provider balances several clients (`sdk/chains/ethereum/src/testing/record.rs:20, 179-180`).
- **Fix:** keep the text match, but keep a corpus of geth + reth + erigon + nethermind strings and test `reason()` over it (074 §7). Effort S.

### F8: Silent no-ops, late validation. P2 · open
- See deep dive 3. Also accepted at create but refused every round: `value` with too many decimals, `count` with a non-whole value, unknown `This{asset}` (075 F8). **Fix:** validate at create (075 §6.6). Effort S.

### F9: Runs API lacks outcome. P2 · open
- `GET /v1/triggers/{id}/runs` returns `started_at` only (`src/api/triggers.rs:441-449`), though the row has `jobs` (`src/models/trigger_run.rs:10-17`).
- **Fix:** return `jobs: [{id, state, reason}]` and `outcome: pending|succeeded|failed|mixed`. Effort S.

### F10: BTC estimate takes a second DB connection. P2 · open
- The round holds a read connection. The BTC estimator calls `find_unspent` via `db.read` (`src/usecase/sdk/inputs.rs:19`) on another connection and another snapshot. The pool is 2..16 by CPU (`src/db.rs:129`). On a 2-CPU host, two BTC-like rounds each hold one connection and wait for a second (r2d2 waits 30 s by default).
- **Fix:** give the round's estimator the round's connection (a store bound to the connection), or read the unspent coins in the round and pass them in. Effort M.

### F11: Round cost vs Solana's interval. P2 · open
- 075 §5: 100k wallets with fund-gas + sweep ≈ 4–6 s of SQL per round. Solana's interval is 1 s (`sdk/chains/solana/src/config.rs:67`). After F1 is fixed, rounds no longer pile up, but triggers decide only every ~5 s and the round holds the network topic (broadcasts wait).
- **Options:** (a) round interval separate from the index interval (e.g. ≥ 5 s for Solana); (b) one select per selector shared by triggers; (c) log round ms and warn when it exceeds the interval (075 §6.8).
- **Recommend:** (c) now, (a) next. Effort S.

---

## P3: hygiene

- **F12, fee per round:** see deep dive 4. Effort S.
- **I2, idle networks never read their tip:** `fetch` returns `Idle` before `tips()` when there is no index and no kept block (`sdk/base/src/indexer.rs:249-251`). `network_index.tip` stays null and the UI shows nothing. Fix: read `tips()` first. Effort S.
- **I6, jobs list and stats scan whole tables:** `q` has only the partial index `q_unfinished` (`init.sql:426`). `find_page` orders by `updated_at` and filters with `json_extract` (`src/models/task.rs:201-247`). Stats count `leg`, `transaction` and `q` in full (`src/models/transaction.rs:318-335`, `leg.rs:451-454`). Fix: an index `q(updated_at, id)`, plus counters kept in `network_index`. Effort S.
- **I9, mislabeled recording:** `eth_sendRawTransaction.underpriced.json` holds `-32003 "insufficient funds … want 21001"` (the key has 0 ETH, and the node checks funds first). Re-record with a funded key, or rename it to `funds.low_fee`. Effort S.
- **R6, `last_failure` has no message:** by design it keeps only the class or code (`packages/rpc/src/statistics.rs:37-39, 120-127`), so `/v1/networks` cannot say *why*. Fix: keep the last truncated message (no URL) beside the code. Effort S.
- **N4 (NEW), `-32003` + "limit":** `value.rs:87` makes any -32003 whose message has "limit" a rate limit. reth uses -32003 for pool rejections (recording above), so a message like "exceeds block gas limit" would likely count as a rate limit and demote the node. Fix: match "rate limit" / "request limit" only. Effort S.
- **N5 (NEW), transport tests missing:** `packages/rpc/tests/` is empty. R1–R4 and R6 have no test on disk. Add `http.rs` (074 §10.4). Effort M.
- **A1, unknown query params ignored:** `IndexParams` (`src/api/mod.rs:244-252`), `jobs::ListQuery` (`src/api/jobs.rs:25-36`) and `network_transactions::ListQuery` (`:27-33`) lack `deny_unknown_fields`, so all 9 paged lists ignore typos (`?status=failed` on `/v1/jobs` lists every job). Two `Query` extractors on one handler cannot both deny. Fix: one flattened query struct per list, with `deny_unknown_fields`. Effort S.
- **A2, `external_id` re-add:** `INSERT … ON CONFLICT DO NOTHING` then fill only a null `external_id` (`src/models/wallet.rs:390-420`). A different id gets 200 with the old one. This is documented (`src/api/wallets.rs:93-95`), but silent. Fix: 409 when both are set and differ. Effort S.

---

## Deep dive 1: "Decide at confirmed"

**What a trigger reads today.** Every compare and every `balance{of}` reads `WalletIndex::spendable` (`src/usecase/trigger.rs:149-152, 186-190`). That is the select query's `balance_spendable`: the balance at `last` − pending outgoing legs (`src/models/wallet_index.rs:713-727`). Today `last` = the newest kept block (`trigger.rs:250-251`), so the number is "tip balance − pending sends". The API shows no such field. After the change it would be exactly the API's `balance_spendable`, the same number the broadcast check uses (`init.sql:249-262`, `src/models/transaction.rs:256-271`).

**What changes, in plain words:**
- **Mined but not confirmed money:** invisible to triggers until N blocks pass. A deposit triggers nothing until it is confirmed, and then only once (with F1 fixed).
- **Pending (mempool) incoming:** never counted, same as today.
- **Pending (mempool) outgoing:** subtracted, same as today. So a sweep sent but not yet mined does not fire again.
- **Sent and mined, not yet confirmed:** *not* covered today (N2). The spend has left "pending" but is not yet below `confirmed`, so the old balance shows for N blocks and a sweep fires again (the node refuses it). **N2 must be fixed first.**
- **Can a trigger fire on money already gone?** With N2 fixed: no. Pending and mined-unconfirmed sends are both subtracted, and the round never runs beside a broadcast of its network (topic). Without N2: yes, for N blocks after each send is mined.

**Delay added (deposit mined → round sees it):**

| chain | depth | wait | with chain's own final instead |
|---|---|---|---|
| ETH | 12 blocks × 12 s | ≈ 2.4 min (+12 s interval) | `finalized` ≈ 2 epochs ≈ 12.8–19 min; `safe` (justified) ≈ 6.4–12.8 min |
| SOL | 32 slots × 0.4 s | ≈ 13 s (+1 s) | `finalized` ≈ the same (max lockout ~32 slots) |
| BTC | depth 6 → tip − 5 | ≈ 50 min (+30 s) | same (BTC has no final tag) |

**Interaction with the other findings:**
- F3 gets **worse** without a gate: the gas transfer stays invisible for N more blocks, so it is re-sent every round until confirmed. Ship the busy gate first or together.
- F4 is fixed: no sweep of funds that the check would refuse. Failed runs no longer use up `since`.
- F5 is fixed: the amount is the confirmed part, and newer deposits sweep later.
- F6 is fixed for reorgs above `confirmed`: the deposit must be final before anyone pays.

**Should ETH use `finalized`/`safe` instead of tip − 12?** Facts: `finalized` blocks cannot be reverted without slashing a third of stake ([ethereum.org PoS](https://ethereum.org/en/developers/docs/consensus-mechanisms/pos/)). Many nodes return the last justified checkpoint for `safe`, and it can stall ([Dwellir](https://www.dwellir.com/blog/ethereum-safe-block-fast-confirmation)). Post-merge mainnet reorgs are almost always 1 block, so tip − 12 is safe in normal times. It is unsafe when finality stalls, which is exactly when tip − 12 keeps advancing (E).

**Recommendation:** decide at `network_index.confirmed` with `confirmed = min(tip − N, chain-reported final)` (E option a). Order: N2 → busy gate → decide at confirmed → E. On ETH that means deposits trigger after ~13–19 min. If that is too slow, make `finality: safe` the per-network option rather than dropping the bound.

## Deep dive 2: "Busy gate" (fix 3)

**What:** one built-in rule. *A trigger does not decide while its last run is still settling.*

**Problem it solves:** a trigger is level-triggered. It sees "the deposit has tokens and no gas" and acts, and it sees the same thing next round, because its own action has not landed yet. That is F3 (gas twice), F4 (a failed sweep repeated every round), and part of F5. Deciding at confirmed alone makes the "not landed yet" period longer.

**How it would work:**
1. The round reads each trigger's `last_run` (it already does, `trigger/mod.rs`, `Triggers::new`).
2. "Busy" = any job of that run is not finished, or any transaction those jobs recorded is pending **or mined above `confirmed`** (N2's window).
3. A busy trigger is skipped this round. Its status says `busy` with the run id (deep dive 3), so it is not silent.
4. A finished run whose jobs all failed is not busy (it may decide again), but `since` ignores it (075 Q3).

**Alternatives:**
- (a) Idempotency key per (trigger, This, block): exact, but keys must survive reorgs; more code.
- (b) Count our own pending incoming legs in the filter (fixes F3 only).
- (c) `since` everywhere: it hides the problem and delays real work.

**Recommend** the gate. It is small (one query per round over `trigger_run.jobs` → `q` and `leg`) and covers F3/F4/F5 at once.

## Deep dive 3: "Make no-ops visible" (fix 5)

**What does nothing today, silently:**
- `send` with an empty source or destination side returns `Ok`: no job, no log (`src/usecase/workers/trigger/actions.rs:195-197`).
- A transfer whose amount is ≤ 0 or unknown is skipped (`actions.rs:202-204`).
- A compare on something unknown is `false`: sibling not fully indexed, no asset, empty `min/max` (`src/usecase/trigger.rs:128-130, 146-148, 160-162`).
- A `select` target never says "not fully indexed". Paused or unindexed `ids` just pick nothing (`src/models/wallet_index.rs:800`).
- No round runs before the first kept block (`trigger.rs:252-255`) or while the network is not at the tip. Rounds are only queued at `Next::Idle` (`index.rs:127-136`), and an enqueue error is dropped (`let _ =`, `index.rs:134`).
- Refusals are a `trigger.refused` warn log only (`trigger/mod.rs:113-127`).

**What "visible" means:**
- **Trigger record:** `last_decision {at, outcome: acted|held|busy|skipped|refused, reason}`, written once per round in the round's single write (F2's fix).
- **Run record:** a run row for `skipped`/`refused` too (or only on change, to limit rows), with `jobs[]` and their states (F9).
- **API:** `GET /v1/triggers/{id}` shows `last_decision`; runs show jobs and outcome.
- **Log:** one `trigger.round` info line per round (ms, triggers, acted, skipped, refused); a warn on refusal, as today.
- **Rules:** empty source/destination → refusal "no source"; unknown amount → refusal; `ids` naming missing/paused indexes → `None` → refusal "not fully indexed".

## Deep dive 4: Fee estimate caching

**What exists.** All three estimators share one factory with the builders (the round uses `service.wallets.factory.fees()`, `trigger/mod.rs:92`). `base::Cache` is a TTL value with single-flight refresh (`sdk/base/src/cache.rs:11-46`).

| chain | cached | TTL (default) | uncached per call |
|---|---|---|---|
| ETH | EIP-1559 fees (`base`, `priority`) | `fee_ttl_ms` 12 s (`sdk/chains/ethereum/src/config.rs:70`, `fee.rs:176-189`) | nothing via RPC: native gas is the constant 21 000, token gas is cached for ever (`fee.rs:161-164, 194-219`) |
| SOL | blockhash + `getFeeForMessage` per signature | `fee_ttl_ms` 5 s (`sdk/chains/solana/src/config.rs:65`, `fee.rs:62-82`) | token-account rent cached for ever (`fee.rs:56, 85-87`) |
| BTC | `estimatesmartfee` rate | `fee_ttl_ms` 60 s (`sdk/chains/bitcoin/src/config.rs:77`, `fee.rs:93-104`) | `find_unspent` DB read **every call** (`fee.rs:129`), on a second connection (F10) |

**Per round, per send.** Each `fee{of}` term calls `estimate` once per use: per kept index in a filter, and per source/destination in the amount (`src/usecase/trigger.rs:192-203`). The `targets` cache holds selections, not fees. So 075's "not cached, each use calls the estimator" is **literally true but misleading**: on ETH/SOL these calls are arithmetic plus at most one RPC per TTL per network. On BTC each one is a DB query.

**Real gaps:**
1. The TTL can expire mid-round, so the filter and the amount see different fees (075 scenario 15). A 100k-wallet ETH round takes 4–6 s vs the 12 s TTL; Solana rounds can exceed the 5 s TTL.
2. The broadcast job builds later with the fee *then* (`fee.rs:145-147`, same cache). If fees rose, `balance − fee` + the new fee exceeds the balance, and the job fails `insufficient-funds` (then F4's `since` problem).
3. BTC: one DB read per use, on another snapshot.

**Is a per-round cache still needed?** Not for RPC load. It is worth it for **consistency**: take one fee snapshot at round start and use it for every term. Also hand the builder a small margin, or the round's fee, so a sweep does not fail on a fee tick. For BTC, read the unspent coins once per round on the round's connection. **Recommend:** a per-round fee snapshot + BTC on the round's connection (075 §6.7). Effort S.

---

Sources: [reth pruning](https://reth.rs/run/faq/pruning) · [ethereum.org PoS finality](https://ethereum.org/en/developers/docs/consensus-mechanisms/pos/) · [Dwellir on `safe`](https://www.dwellir.com/blog/ethereum-safe-block-fast-confirmation) · [Tatum block tags](https://docs.tatum.io/docs/evm-block-finality-and-confidence) · [Infura eth_getLogs limits](https://docs.metamask.io/services/reference/base/json-rpc-methods/eth_getlogs) · [geth core/error.go](https://raw.githubusercontent.com/ethereum/go-ethereum/master/core/error.go) · [reth pool errors](https://reth.rs/docs/reth_rpc_eth_types/error/enum.RpcPoolError.html) · [Solana commitment levels](https://docs.rs/solana-commitment-config/latest/solana_commitment_config/enum.CommitmentLevel.html) · live probes 2026-11-04: `ethereum-sepolia-rpc.publicnode.com` (reth 2.8.0; receipts kept 9 000, `null` at 10 800 blocks back), `ethereum-rpc.publicnode.com` (receipts 0.48–0.55 MB, full block 0.31 MB), `api.{mainnet-beta,devnet}.solana.com` (`getBlock` v0 → -32015 on every block sampled).
