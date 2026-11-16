# Known issues (2026-11-10)

Paths under `apps/server/` unless they start with `packages/` or `sdk/`. Line numbers are from the
2026-11-04 tree and may have shifted; a fixed item is removed; a partly fixed one says what is left.
Priorities: **P0** money/safety · **P1** correctness/data loss · **P2** availability/perf · **P3** hygiene.
NEW = found while checking this list.

## Summary

| id | P | one line | status |
|---|---|---|---|
| H1 | P2 | A network's depth limit (`max_depth`) is set per network; how far back a node serves is the node's, so its RPC endpoint should set it | open |
| D | P2 | A node answering garbage is never failed over (744 failed steps in 5 s) | open |
| T1 | P2 | One 15 s timeout for everything. No deadline for a step or a send; boot can block | partly fixed (13dda2f, 6839bcd) |
| R1 | P2 | Short batch returns `Ok` | partly fixed (6839bcd) |
| R4 | P2 | HTTP 500 with a JSON-RPC body loses the node's code (send: "outcome unknown") | open |
| R5 | P2 | `-32005 more than 10000 results` counts as a rate limit; the range is not split | open |
| I3 | P2 | Ethereum downloads all receipts of every block (~0.5 MB per mainnet block) | open |
| I4 | P2 | Solana downloads whole blocks (~2 MB each, 2.5 per second) | open |
| I5 | P2 | The network transactions list scans and sorts all transactions | open |
| I8 | P2 | Send refusals are classified by message text, and the wording differs by client | open |
| F8 | P2 | Silent no-ops, and bad values are only refused when a round runs | partly fixed (b6eda5e) |
| F10 | P2 | BTC fee estimate takes a second DB connection inside the round | open |
| F11 | P2 | Round cost at 100k wallets (4–6 s) is more than Solana's 1 s interval | open |
| F12 | P3 | Fee estimate is not fixed per round, so the TTL can expire mid-round or before broadcast | open |
| I6 | P3 | The jobs list and stats scan whole tables | open |
| I9 | P3 | `eth_sendRawTransaction.underpriced.json` actually holds "insufficient funds" | open |
| R6 | P3 | `Statistics.last_failure` has no message | open |
| A1 | P3 | Unknown query params are silently ignored on every paged list | open |
| A2 | P3 | Re-adding a wallet with a different `external_id` silently keeps the stored one | open |

---

## P2: availability and performance

### D: Garbage answers never fail over. P2 · open
- **What/when:** A result that decodes as nothing asked is still a JSON-RPC success for `Http`/`Cluster` (`packages/rpc/src/testing/faults.rs:149,167`). `observe` only counts transport failures (`cluster.rs:234-245`). The decode error happens above the cluster (`sdk/chains/ethereum/src/rpc.rs:689-697`). The case run showed 744 failed steps in 5 s at the test's 5 ms interval: one failed step per interval for ever.
- **Options:** (a) the chain reports "bad answer" to the client (`Client::fault(node)`), and the cluster counts it as a transport failure; (b) decode inside `Http` against a schema (heavy); (c) the index job backs off exponentially on repeated equal errors.
- **Recommend:** (a), plus (c) as a storm guard. Effort M.

### T1: RPC timeouts and deadlines (user question). P2 · partly fixed (13dda2f, 6839bcd)
- **Partly fixed (13dda2f, 6839bcd):** Boot asks no node. Requests and batches carry their own timeout: a batch 5 s + 1 s per call, a send 10 s per node, a tip probe 3 s; the queue bounds a whole job run (its policy timeout, under a lease). Open: per-method budgets for single reads, and a send returning at the first acceptance.
- **Today:** One `timeout_ms` per HTTP execution, default 15 s (`packages/rpc/src/endpoint.rs:47-68`, applied in `http.rs:53`). It is the same for every method and every batch: a 2-call `tips` batch and a 10-block receipts batch (~5 MB on mainnet, measured) get the same 15 s. Each chain wraps `Retry::new(Cluster::new(…))` (`sdk/chains/ethereum/src/config.rs:159-162`, same for SOL/BTC). Retry: 3 attempts, 250 ms then 500 ms backoff (cap 2 s) (`retry.rs:35-43, 96-106`). On a due reselection, a probe of all nodes runs inside the call (`cluster.rs:219-228, 249-265`), with up to 15 s more per attempt.
- **One logical call, worst case:** 3 × 15 + 0.25 + 0.5 = **45.75 s**. With a re-probe in each attempt: **~90.75 s**.
- **One ETH indexer step:** at least 4 logical calls in sequence: `tips` batch, blocks batch, receipts batch, `eth_getLogs` (`rpc.rs:173-220`). Log splitting adds calls (up to 19 for 10 blocks), and `null` receipts add a second `tips`. A step that ends in success after 2 timeouts per call: 4 × 45.75 ≈ **3 min**. With the default `batch` 100 instead of seed's 10, mainnet is ~30 MB of blocks + ~50 MB of receipts per step (measured ~0.3 MB + 0.5 MB per block), close to the 64 MiB response cap. `ResponseTooLarge` is not retryable, so the step would fail every interval. An index job runs up to `STEPS = 10` steps (`index.rs:35, 111`). No deadline wraps a step or a job (`usecase/workers/executor.rs` has none; `tasks` deadline means "not started after", `packages/tasks/src/rules.rs:47-50`). `skip` adds ~log2(range) full block reads.
- **Send:** It has no separate timeout. `eth_sendRawTransaction` is `Request::once`, so Retry does not repeat it (`retry.rs:136-138`). The cluster's fan-out waits for **every** node (`cluster.rs:188-216`), so one hanging node adds 15 s to every send. A refusal then calls `known()` (retryable, ≤45.75 s) (`sdk/chains/ethereum/src/rpc.rs:368-408`). Worst case ≈ 15 + 45.75 s, plus nonce/fee reads.
- **Boot:** `Cluster::new` probes before returning (`cluster.rs:145`). Networks open one after another (`sdk/payments/src/payments.rs:327-345`), so a black-holed node costs 15 s per network before the server serves.
- **Options:** (a) per-method bounds: probe/`tips`/`chainId` 3 s; single reads 5–10 s; batches `5 s + 1 s × calls`; send 10 s per node; (b) a total budget per logical call (e.g. 20 s across attempts) instead of attempts × timeout; (c) a step deadline: `timeout(max(2 × interval, 30 s), indexer.next())` → the job's reason is "step deadline"; (d) fan-out returns on the first acceptance after a short grace (e.g. 2 s) and lets the rest finish in the background; (e) probe at boot in the background (first node primary until done).
- **Recommend:** (c) + (b) first (small, bounds everything), then (a) for probes and batches, then (e). Keep send's per-node bound at 10 s with (d). Effort M.

### R1: Short batch returns `Ok`. P2 · partly fixed (6839bcd)
- **Partly fixed (6839bcd):** Not in `Http` (the count check there was removed): callers check. Ethereum and Solana do; Bitcoin `getblockhash` does not.
- jsonrpsee sizes the answer vector by the number of responses received (`jsonrpsee-http-client-0.24.11/src/client.rs:436-442`). `Http::execute_batch` passes it on (`packages/rpc/src/http.rs:116-141`). ETH and SOL check the length (`sdk/chains/ethereum/src/rpc.rs:525-529`, `sdk/chains/solana/src/rpc.rs:388`). BTC `getblockhash` does not (`sdk/chains/bitcoin/src/rpc.rs:228-238`): a short answer looks like "chain ends here".
- **Fix:** check `results.len() == calls.len()` in `Http` → `InvalidResponse`. Effort S.

### R4: HTTP 500 with a JSON-RPC body loses the code. P2 · open
- jsonrpsee drops the body on non-2xx (`transport.rs:334-337`), so it becomes `HttpStatus(500)` (`http.rs:204-207`). For `eth_sendRawTransaction` that is a transport error. `send` then says "outcome unknown" (`sdk/chains/ethereum/src/rpc.rs:377-381`) and the job is nacked and repeated, although the node said "nonce too low".
- **Fix:** a custom transport layer that parses a JSON-RPC body on 4xx/5xx; or a raw `reqwest` for broadcasts. Effort M.

### R5: `-32005 more than 10000 results` is not split. P2 · open
- `is_rate_limited` treats every `-32005` as a limit (`packages/rpc/src/value.rs:84-90`). `Http` turns it into `RateLimited` (`http.rs:97`). `transfers` only splits on error objects or `ResponseTooLarge` (`sdk/chains/ethereum/src/rpc.rs:478-489`), so the step fails, the primary is demoted, and the same range fails again. Infura answers both "more than 10000 results" and "query timeout exceeded" with -32005 ([Infura eth_getLogs](https://docs.metamask.io/services/reference/base/json-rpc-methods/eth_getlogs)).
- **Fix:** `Failure::is_too_much()` (message has "more than", "range", "too many", "size") checked before `is_rate_limited`; splitting treats `RateLimited` with such text as "split". Effort S.

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

### F8: Silent no-ops, late validation. P2 · partly fixed (b6eda5e)
- **Partly fixed (b6eda5e):** A count compares with a whole number, refused at create. Open: the silent no-ops, and values checked against the network's assets (left to the round on purpose).
- See deep dive 3. Also accepted at create but refused every round: `value` with too many decimals, `count` with a non-whole value, unknown `This{asset}` (075 F8). **Fix:** validate at create (075 §6.6). Effort S.

### F10: BTC estimate takes a second DB connection. P2 · open
- The round holds a read connection. The BTC estimator calls `find_unspent` via `db.read` (`src/usecase/sdk/inputs.rs:19`) on another connection and another snapshot. The pool is 2..16 by CPU (`src/db.rs:129`). On a 2-CPU host, two BTC-like rounds each hold one connection and wait for a second (r2d2 waits 30 s by default).
- **Fix:** give the round's estimator the round's connection (a store bound to the connection), or read the unspent coins in the round and pass them in. Effort M.

### F11: Round cost vs Solana's interval. P2 · open
- 075 §5: 100k wallets with fund-gas + sweep ≈ 4–6 s of SQL per round. Solana's interval is 1 s (`sdk/chains/solana/src/config.rs:67`). Rounds no longer pile up, but triggers decide only every ~5 s and the round holds the network topic (broadcasts wait).
- **Options:** (a) round interval separate from the index interval (e.g. ≥ 5 s for Solana); (b) one select per selector shared by triggers; (c) log round ms and warn when it exceeds the interval (075 §6.8).
- **Recommend:** (c) now, (a) next. Effort S.

---

## P3: hygiene

- **F12, fee per round:** see deep dive 4. Effort S.
- **I6, jobs list and stats scan whole tables:** `q` has only the partial index `q_unfinished` (`init.sql:426`). `find_page` orders by `updated_at` and filters with `json_extract` (`src/models/task.rs:201-247`). Stats count `leg`, `transaction` and `q` in full (`src/models/transaction.rs:318-335`, `leg.rs:451-454`). Fix: an index `q(updated_at, id)`, plus counters kept in `network_index`. Effort S.
- **I9, mislabeled recording:** `eth_sendRawTransaction.underpriced.json` holds `-32003 "insufficient funds … want 21001"` (the key has 0 ETH, and the node checks funds first). Re-record with a funded key, or rename it to `funds.low_fee`. Effort S.
- **R6, `last_failure` has no message:** by design it keeps only the class or code (`packages/rpc/src/statistics.rs:37-39, 120-127`), so `/v1/networks` cannot say *why*. Fix: keep the last truncated message (no URL) beside the code. Effort S.
- **A1, unknown query params ignored:** `IndexParams` (`src/api/mod.rs:244-252`), `jobs::ListQuery` (`src/api/jobs.rs:25-36`) and `network_transactions::ListQuery` (`:27-33`) lack `deny_unknown_fields`, so all 9 paged lists ignore typos (`?status=failed` on `/v1/jobs` lists every job). Two `Query` extractors on one handler cannot both deny. Fix: one flattened query struct per list, with `deny_unknown_fields`. Effort S.
- **A2, `external_id` re-add:** `INSERT … ON CONFLICT DO NOTHING` then fill only a null `external_id` (`src/models/wallet.rs:390-420`). A different id gets 200 with the old one. This is documented (`src/api/wallets.rs:93-95`), but silent. Fix: 409 when both are set and differ. Effort S.

---

## Deep dive 3: "Make no-ops visible" (fix 5)

**What does nothing today, silently:**
- `send` with an empty source or destination side returns `Ok`: no job, no log (`src/usecase/workers/trigger/actions.rs:195-197`).
- A transfer whose amount is ≤ 0 or unknown is skipped (`actions.rs:202-204`).
- A compare on something unknown is `false`: sibling not fully indexed, no asset, empty `min/max` (`src/usecase/trigger.rs:128-130, 146-148, 160-162`).
- A `select` target never says "not fully indexed". Paused or unindexed `ids` just pick nothing (`src/models/wallet_index.rs:800`).
- No round runs before the first kept block (`trigger.rs:252-255`) or while the network is not at the tip. Rounds are only queued at `Next::Idle` (`index.rs:127-136`), and an enqueue error is dropped (`let _ =`, `index.rs:134`).
- Refusals are a `trigger.refused` warn log only (`trigger/mod.rs:113-127`).

**What "visible" means:**
- **Trigger record:** `last_decision {at, outcome: acted|held|skipped|refused, reason}`, written once per round in the round's single write.
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
