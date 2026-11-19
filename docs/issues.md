# Known issues (2026-11-19)

Found by a five-part QA run against the live server on 2026-11-18 (API, sends, indexing, triggers
and jobs, UI), on the real database, testnets only, plus a check of the 2026-11-10 list against the
code at 3247ee6. Paths are under `apps/server/` unless they start with `packages/` or `sdk/`.
Priorities: **P0** money/safety · **P1** correctness/data loss · **P2** availability/perf/operability ·
**P3** hygiene. A fixed item is removed; a partly fixed one says what is left.

Not covered by the run: Solana sends (no funded, indexed devnet wallet; the faucet is closed and
the devnet nodes are rate-limited), live bumps (nothing got stuck), Bitcoin (no Bitcoin network is
configured), real reorgs, auth (loopback needs no token), and UI rendering (no browser).

## Fixed since the QA run (unstaged on 2026-11-19)

- **S1:** the Ethereum builder signs each new payment with the node's `eth_estimateGas` for it
  (plus the margin) and refuses one that would revert; the per-kind gas cache serves estimates
  (triggers, planning) only.
- **S2:** a version found reverted fails the job `reverted` (what is left is above, P2).
- **S3:** not a bug: a pause only stops tracking. Sends from a paused index work, and the sent
  transaction is recorded as mined with its legs for every index behind its block; on resume the
  settle adjusts only what moved outside them. A client sees a current balance when `state` is
  `indexing` and `indexed_through >= finality_at`.
- **S4:** a send with no index for the asset (or, for a token, no native index) is refused at the
  API with 422 `not-indexed`.
- **X1:** Solana's line is `Line::Depth(read_depth)` (setting, default 1 000; devnet 150): final
  blocks are read again; an index further behind settles by one balance read.
- **X2:** the cause was Tatum's free plan answering `-16401` "paid plans only" to balance reads,
  taken as the final answer; a node that does not serve a method (`-16401`, `-32601`) now passes the
  call to the next node, and a failed close reaches the index job's reason.

## Summary

| id | P | one line |
|---|---|---|
| X4 | P1 | Ether sent by contracts is never seen; balances drift silently (57 ETH on one watch-only wallet) |
| G1 | P1 | A daily trigger whose payment failed counts as run and skips that payment for 24 h |
| G2 | P1 | `PUT /v1/triggers/{id}` without `enabled` turns a paused trigger back on |
| G3 | P1 | A `not` selector gets around the "would select every wallet" guard |
| U1 | P1 | The UI treats every 204 as an error; adding a wallet with tags creates orphan wallets |
| U2 | P1 | The UI opens a wallet on its token, not native coin, and Send defaults to it |
| S2 | P2 | A reverted payment settled through the `Confirmer` (its block not read) still ends done |
| X3 | P2 | Indexing failures: a failed close now reaches the index job's reason; the wallet index's own reason, a log and recent counts are still missing |
| X5 | P2 | `/v1/stats` jobs `failed` shows the idle count (691–1124 instead of 25) |
| X6 | P2 | A new Solana index shows balance `"0"` instead of null until its first settle |
| X7 | P2 | The devnet nodes are exhausted: the only `getBlock` route answers 429 "max usage reached" |
| S5 | P2 | Refusals drop the node's message and the parsed reason (`code -32000` only) |
| G4 | P2 | Trigger definitions that cannot work are accepted with 201 (amounts, ids, networks, `since`) |
| G5 | P2 | A failing trigger without `since` queues a failed job every round; failed jobs are never pruned |
| G6 | P2 | A trigger round runs every block on every network, even with no triggers (~50k idle rows a day) |
| U3 | P2 | The UI send dialog waits forever after a bump, a replaced or a dead transaction |
| U4 | P2 | The committed `ui/dist` is stale: no fee choice, so sends from :8080 always use `normal` |
| A3 | P2 | `POST /v1/wallets/{id}/transactions` accepts amounts beyond uint256 and `"+1"`; the job fails later |
| A4 | P2 | Index `start` above i64 gives 503 with SQLite's error text; a future `start` is accepted |
| H2 | P2 | A pruned primary node moves an index's start up although another node serves those blocks |
| D | P2 | A node answering garbage is never demoted; the index job fails every run (linear backoff) |
| T1 | P2 | Single reads use a 15 s default; Retry × every node × 15 s; a send waits for every node; no step deadline |
| R1 | P2 | `Http` and Bitcoin `getblockhash` do not check a batch's answer count |
| R4 | P2 | HTTP 500 with a JSON-RPC body loses the node's code (send: "outcome unknown") |
| R5 | P2 | `-32005` too many results is retried as a rate limit and the node demoted; the range is never split |
| I3 | P2 | Ethereum downloads all receipts and full blocks |
| I4 | P2 | Solana reads one `getBlock` per slot; no per-address following |
| I5 | P2 | The network transactions list scans and sorts all transactions |
| I8 | P2 | Send refusals are classified by message text; no erigon or nethermind wording |
| F8 | P2 | Silent trigger no-ops, and values checked only when a round runs (see G4, deep dive 3) |
| F10 | P2 | BTC fee estimate takes a second DB connection inside the round |
| F11 | P2 | Round cost at 100k wallets (4–6 s) is more than Solana's 1 s interval |
| F12 | P3 | No fee snapshot per round; a sweep fails if the rate rises before the job signs |
| U5 | P3 | UI lists read only their first page; "Older" is undone by the 10 s refresh |
| A5 | P3 | Bad path params, unknown routes and body errors: plain-text or empty bodies, one generic message |
| A6 | P3 | `GET /v1/jobs` returns `input: null`, but the schema says object |
| A7 | P3 | Parallel index creates all answer 201 |
| A8 | P3 | Endpoint add calls any URL (loopback, private IPs) with a caller-set, uncapped timeout |
| I6 | P3 | The jobs list and stats scan whole tables |
| I9 | P3 | `eth_sendRawTransaction.underpriced.json` actually holds "insufficient funds" |
| R6 | P3 | RPC statistics keep the last failure's code, not its message |
| A1 | P3 | Unknown query params are silently ignored on every paged list |
| A2 | P3 | Re-adding a wallet with a different `external_id` silently keeps the stored one |
| Q1 | P3 | Leftovers and small inconsistencies (stale jobs, old formats, cosmetic) |

---

## P0: money and safety

None open (S1, S2, S3, X2 fixed; see the top).

---

## P1: correctness and data loss

### X4: Ether sent by contracts is never seen. P1
- **What/when:** internal transfers (a contract paying out ether) appear in no transaction field or
  log, so the indexer misses them (documented in `sdk/chains/ethereum/README.md`). Nothing detects
  the drift.
- **Seen:** watch-only `0x956962c34687a954e611a83619abaa37ce6bc78a` is 57.3 ETH below the chain;
  37 non-zero settle adjustments were written for it.
- **Fix:** a periodic balance check for indexes at the line (one `eth_getBalance` per index per N
  minutes), writing an adjustment and flagging the index ("balance moved without a transaction").
  Effort M.

### G1: A failed daily payment is skipped for 24 h. P1
- **What/when:** `since` counts a run whose job failed, so a daily trigger whose send failed waits
  a full day. Before 084, seed-usdc-3/4/5 failed most days on a reused nonce (jobs 44770, 44772,
  91246–7, 103626–8, 128738–40); today all four landed with consecutive nonces, but any failure still
  skips a day.
- **Fix:** `since` counts the last run whose jobs succeeded, or a failed run's trigger is retried
  after a short backoff. Effort S.

### G2: A PUT without `enabled` re-enables a paused trigger. P1
- **What/when:** `#[serde(default = "enabled")]` (`api/triggers.rs:102-106`): a PUT that leaves the
  field out sets it to true, and the trigger runs in the next round.
- **Fix:** PUT keeps the stored value when the field is absent, or the field is required. Effort S.

### G3: A `not` selector selects every other wallet. P1
- **What/when:** an empty `all` is refused because it "would select every wallet", but
  `{"not":{"tag":"deposit"}}` selects every wallet without that tag, the treasury included. A sweep
  with it empties the treasury.
- **Fix:** refuse a selector whose positive part is empty (`not` alone, `any` of only `not`s).
  Effort S.

### U1: The UI treats every 204 as an error. P1
- **What/when:** `ui/src/api/client.ts:62-72` throws when `data` is undefined, which openapi-fetch
  returns for 204. Adding and removing tags, deleting a trigger and adding a wallet with tags all
  show "204 No Content" in red while the server did it. Adding a wallet with tags then stops before
  step 2; "Next" again generates another keyed wallet, so orphan wallets pile up, never indexed.
- **Fix:** treat 204 as success with no data. Effort S.

### U2: The UI opens a wallet on its token. P1
- **What/when:** `ui/src/money.ts:32` (`assetOf`) takes the first key of `indexes`; the server sorts
  keys, so a token (`0x1c7d…`, `4zMM…`) comes before `native`. The wallet view and Send default to
  the token; the wallet list's currency and balance filters judge by it (`rail.tsx:84`, `93`).
- **Risk:** a user sends the token believing it is the native coin.
- **Fix:** native first, then tokens in a stable order. Effort S.

---

## P2: availability, performance and operability

### S2: A reverted payment through the `Confirmer`. P2 · partly fixed
- **Fixed:** a version found reverted in a final block is `TransactionStatus::Reverted` and the job
  fails `reverted` (`sdk/base/src/index/transaction.rs`, `broadcast.rs`).
- **Left:** `reverted` comes from a mined `transaction` row. A version settled through the chain's
  `Confirmer` (its block kept without transactions) stays pending, reads `Final`, and the job ends
  done. **Fix:** the `Confirmer` reports the transaction's outcome. Effort S.

### X3: Indexing failures are invisible. P2 · partly fixed
- **Fixed:** a failed close names its index and reaches the index job's `reason`
  (`index_job_reason` on `/v1/networks`); the rest below is open.
- **What/when:** gap-close failures are counted (`sdk/base/src/indexer.rs` `fetch` → `Stats::fail`)
  but do not fail the step, so the job's `reason` stays null; `Stats.error` is written but nothing
  reads it; `/health` shows a lifetime `failures` count only (devnet 1648, sepolia 301, mainnets 227
  and 181), while its doc says the reason is on the index job. The server logs only refused triggers
  (`usecase/workers/trigger/mod.rs:121`); RPC failures have no sink (`usecase/transport.rs`,
  `Nodes::open`). `indexed` in `/health` updates only from blocks read, never from settles.
- **Fix:** keep the last error per network and per index (time, message), show it in `/health` and
  on the wallet index; log one line per failed step; count recent failures, not lifetime. Includes
  R6. Effort M.

### X5: `/v1/stats` counts idle jobs as failed. P2
- `usecase/stats.rs:125-133`: statuses 3, 4 and 5 all fall to `_ => jobs.failed = n`, which
  overwrites, so idle (5) wins. Seen: 691 and 1124 against 25 failed. **Fix:** a field per state,
  summed. Effort S.

### X6: A new Solana index shows `"0"` before its first settle. P2
- The settle leg sits above the server's confirmed position, and the `wallet_index_with_balance`
  view turns "no leg" into `'0'`; its doc promises null. Seen for about 12 s on a new index. A
  trigger reading it sees an empty wallet. **Fix:** null until the first leg at or below confirmed.
  Effort S.

### X7: The devnet nodes are exhausted. P2
- `getBlock` is routed only to Helius, which answers 429 "max usage reached" (137 of 138 calls);
  Tatum's free tier allows 3 requests a second; public devnet answered 429 on 159 of 485 calls. The
  transport falls back but retries Helius every 30 s; `/endpoints` shows only "HTTP 429".
  **Fix:** configuration (another `getBlock` node); show the node's message (R6). Effort S.

### S5: Refusals drop the node's message. P2
- `sdk/chains/ethereum/src/rpc.rs:366-372` puts only the code in the message; `broadcast.rs:354`
  keeps only `error.message`, so the parsed `Reason` is lost. Jobs read "refused: Ethereum node
  refused the transaction with code -32000" for "intrinsic gas too low" and "nonce too low" alike.
  **Fix:** keep the node's text and the reason in the job's reason. Effort S.

### G4: Trigger definitions that cannot work are accepted. P2
- Accepted with 201: amounts negative, zero, `1e80` or with more decimals than the asset; selector
  ids that do not parse, do not exist or are on another network (`"garbage"`, `0xZZ`); a source
  index in another asset; a watch-only source; a compare on an unconfigured asset; a `since` above
  about 1.8e16 s (overflows to ms: it runs once, then every round fails "a wait is out of range"); a
  PUT that moves a trigger to another network while its ids stay on the old one.
- `Value::validate` (`sdk/trigger/src/amounts.rs:52`) and `Since::validate`
  (`sdk/trigger/src/conditions.rs:175`) return `Ok`; `selection.rs:213` checks only that ids are
  non-empty. Bad amount errors read "data did not match any variant of untagged enum Form".
- **Fix:** validate at create and update against the network's assets and wallets. Covers F8's
  late validation. Effort M.

### G5: Failed jobs pile up. P2
- A trigger with no `since` whose send fails queues the same failed job every round (devnet: a
  round about every 5 s, about 17k rows a day); `prune.rs:31` prunes only `triggers` jobs and
  `task.rs:298` deletes states 2/4/5 only. **Fix:** prune failed broadcast jobs after a retention;
  hold a trigger after N failed jobs in a row. Effort S.

### G6: Trigger rounds run on networks with no triggers. P2
- In 4 minutes: 152 idle rounds on solana:mainnet and 21 on ethereum:mainnet; about 50k rows a day.
  **Fix:** queue a round only when the network has an enabled trigger. Effort S.

### U3: The UI send dialog can wait forever. P2
- `send.tsx:79-94`: after "Accepted" it stops reading the job and waits for a mined leg with the
  first txid. After a bump (new txid), a replaced or dead transaction, or a failed job it shows
  "Waiting to be mined" for ever; a `skipped` job spins while queued; the job's reason and id are
  never shown. **Fix:** follow the job until it ends, show its txid and reason. Effort S.

### U4: The committed UI is stale. P2
- `ui/dist/app.js` has no fee choice (built at 521d66f; the fee select came in 3247ee6), so sends
  from the page the server serves always use `normal`. `GET /ui/` also answers 404 on the server
  (`/ui` and `/ui/wallets` are 200). **Fix:** rebuild and commit dist; serve `/ui/`. Effort S.

### A3: Send amounts are checked late. P2
- `"9999…"` (80 digits) and `"+1"` get 202 and the job fails "Ethereum amount must be a positive
  256-bit number" (`usecase/wallets.rs:359-367`). An off-curve Solana recipient also gets 202 and
  fails in the job (143565). **Fix:** bound the amount by the chain and parse the recipient fully at
  the API. Effort S.

### A4: Index `start` is unbounded. P2
- `start` above i64::MAX → 503 "a value is above SQLite's integer range…"
  (`api/wallet_indexes.rs:48`, `Option<u64>`); i64::MAX or any future height → 201 with
  `requested_from` 9223372036854775807. **Fix:** 400 above the tip. Effort S.

### H2: A pruned primary node moves the start. P2
- What is left of the old H1: a node that keeps no history answers `NotServed`, which moves an
  index's start up even when another node serves those blocks (case `pruned_one` ignored,
  `tests/cases/mod.rs:58-60`). **Fix:** try another node before moving the start. Effort M.

### D: A node answering garbage is never demoted. P2
- `observe` counts only retryable transport failures; a garbage answer decodes at the transport and
  fails above it as `InvalidResponse` (`usecase/transport.rs:252-263, 423-429`,
  `packages/rpc/src/value.rs:31-41`). The queue now waits 1 s × attempts between failed runs
  (`usecase/workers/mod.rs:23-26`, `packages/tasks/src/job.rs:183`), so it is no longer a storm.
  Case `broken` ignored (`tests/cases/mod.rs:61-63`). **Fix:** the chain reports a bad answer to the
  transport, which demotes the node. Effort M.

### T1: Timeouts and deadlines. P2 · partly fixed
- Done: boot asks no node; batches, sends and tip probes have their own bounds
  (`packages/rpc/src/client.rs:162-169`, `usecase/transport.rs:43`).
- Left: single reads use the 15 s default (`packages/rpc/src/http.rs:43, 111`); Retry wraps the node
  walk, so the worst case is 3 attempts × every node × 15 s (`transport.rs:252-263, 286`); a send
  waits for every node (`:270-282`); no step deadline, only the 5 min run timeout
  (`usecase/workers/index.rs:40, 156-180`). **Fix:** a step deadline and a total budget per call;
  the send returns at the first acceptance. Effort M.

### R1: Batch answer count unchecked. P2 · partly fixed
- `Http` (`packages/rpc/src/http.rs:136-166`) and Bitcoin `getblockhash`
  (`sdk/chains/bitcoin/src/rpc.rs:188-203`) do not check it; jsonrpsee sizes the answers by what
  arrived. Ethereum, Solana and Bitcoin `getblock` check. **Fix:** check in `Http`. Effort S.

### R4: HTTP 500 with a JSON-RPC body. P2
- A non-2xx becomes `HttpStatus(500)` (`packages/rpc/src/http.rs:261-264`), and the Ethereum send
  says "outcome unknown" (`sdk/chains/ethereum/src/rpc.rs:345-353`) although the node said why.
  **Fix:** parse a JSON-RPC body on 4xx/5xx. Effort M.

### R5: `-32005` too many results. P2
- Every `-32005` is a rate limit (`packages/rpc/src/error.rs:229-236`); the log fetch does not split
  a rate-limited range (`sdk/chains/ethereum/src/rpc.rs:472-477`); the node is demoted
  (`usecase/transport.rs:426, 431-439`). **Fix:** "more than", "range", "too many" means split.
  Effort S.

### I3: Ethereum downloads all receipts. P2
- Receipts for every non-empty block and full blocks (`sdk/chains/ethereum/src/rpc.rs:114-135,
  416-434, 615-628`). **Fix:** receipts only for transactions of watched addresses. Effort M.

### I4: Solana reads every slot. P2 · partly fixed
- `transactionDetails: accounts` is in (7e1e9a9); still one `getBlock` per slot
  (`sdk/chains/solana/src/rpc.rs:118-135`). **Fix:** follow watched addresses with
  `getSignaturesForAddress`. Effort L.

### I5: Network transactions list scans everything. P2
- `find_page` orders by expressions over the legs view; the only index on `transaction` is on
  `block` (`models/transaction.rs:419-458`, `init.sql:73-89`). **Fix:** an index on
  `(chain, network, block)` and keyset paging. Effort S–M.

### I8: Refusals by message text. P2 · partly fixed
- A unit test covers geth and reth wording (`sdk/chains/ethereum/src/rpc.rs:748-770`); matching is
  still by text (`:695-716`), with no erigon or nethermind strings. See S5. Effort S.

### F8: Silent no-ops, late validation. P2 · partly fixed
- Done: the send's asset is checked at create (`usecase/workers/trigger/actions.rs:54-61`); a count
  compares with a whole number (`sdk/trigger/src/conditions.rs:144-153`). Left: see G4 and deep
  dive 3.

### F10: BTC estimate takes a second DB connection. P2
- The round holds a read connection; the BTC estimator reads unspent coins on another
  (`usecase/workers/trigger/mod.rs:97-113`, `sdk/chains/bitcoin/src/fee_estimator.rs:54`,
  `usecase/sdk/inputs.rs:19`; pool `db.rs:170-173`). **Fix:** the round's connection. Effort M.

### F11: Round cost vs Solana's interval. P2
- Solana's index interval is 1 s (`sdk/chains/solana/src/config.rs:73`); a round is queued at every
  idle step (`usecase/workers/index.rs:171-175`); nothing logs round time. With G6. Effort S.

---

## P3: hygiene

- **F12, no fee snapshot per round:** each fee term calls the estimator
  (`sdk/trigger/src/amounts.rs:137`) over the shared rate cache (`sdk/base/src/fee.rs:163-193`;
  TTL 12 s ETH, 5 s SOL, 60 s BTC). The builder takes the rate when it signs
  (`sdk/chains/ethereum/src/builder.rs:110-120`), so a sweep fails `insufficient-funds` when the rate
  rises in between. Also: the trigger `fee` amount is 2.4–2.9× the actual cost (it is the signed
  worst case, gas limit × max fee), so `balance − fee` sweeps leave dust. Effort S.
- **U5, UI lists:** jobs (first 100 of ~191; the selected job vanishes when it drops off,
  `jobs/page.tsx:104, 117`), block transactions and mempool, triggers, runs and tags read one page;
  "Older" movements are replaced by the 10 s refresh (`wallet.tsx:138-171`); responses of a previous
  asset can overwrite a new tab; a null balance shows "0" (`wallet.tsx:46, 51`); no error boundary;
  the selected wallet is not in the URL; a cancelled Add wallet keeps the typed key. Effort M.
- **A5, error shapes:** a bad path param → 400 `text/plain` not in the spec (`api/network_blocks.rs:81`,
  `api/network_endpoints.rs:242`; only Json and Query rejections are mapped, `api/mod.rs:158-166`);
  unknown routes and wrong methods → empty bodies; every body error is one generic message with no
  field, no 413 or 415; error messages echo the whole input. Effort S.
- **A6, jobs schema:** prune jobs return `"input": null`; `Job.input` is `type: object`. Effort S.
- **A7, index create race:** 8 parallel creates all answer 201 (check then insert,
  `usecase/wallets.rs:195-205`). Effort S.
- **A8, endpoint add:** calls any URL (loopback, private IPs) and the caller can set
  `rpc.timeout_ms` without a cap (40 s seen). Node numbering is mixed 0-/1-based, a duplicate URL
  reads "listed twice", and a multi-network add does not name the network. Effort S.
- **I6, full scans:** the jobs list orders by `updated_at` and filters with `json_extract`, with no
  index (`models/task.rs:351-392, 316-324`, `init.sql:541-545`); stats count whole tables
  (`models/transaction.rs:375-391`, `models/leg.rs:671-673`). Effort S.
- **I9, mislabeled recording:** `sdk/chains/ethereum/src/testing/recordings/eth_sendRawTransaction.underpriced.json:11`
  holds "insufficient funds … want 21001". Effort S.
- **R6, no failure message:** `packages/rpc/src/decorators/counted.rs:24, 79-84, 98` keeps only the
  code. Part of X3. Effort S.
- **A1, unknown query params ignored:** `api/mod.rs:217-225`, `api/jobs.rs:24-35`,
  `api/network_transactions.rs:27-33` (the jobs filter is `state`, so `?status=` lists everything).
  Effort S.
- **A2, `external_id` re-add:** filled only when null (`models/wallet.rs:476-489`); a different id
  gets 200 with the old one. **Fix:** 409. Effort S.
- **Q1, leftovers and small inconsistencies:**
  - four `expire` jobs (ids 2, 4, 5, 6) stay pending for ever: the worker was removed in 521d66f
    without a cleanup (`084-migrate-1.sql`);
  - 42 broadcast jobs from before 084 show `txid: null, accepted: false` (old progress shape,
    `broadcast.rs:637`); three `triggers` jobs (7415, 7444, 7458) fail "missing field `selected`";
  - a transaction's `reason` is double-encoded (`"\"reverted\""`);
  - failed jobs show a `run_at` about 65 s ahead;
  - after a self-send `spendable` is below `pending` until final (the incoming leg is not counted);
  - `PUT /v1/triggers/{id}` returns `last_round: null` (`usecase/triggers.rs:173`);
  - `GET /v1/triggers/{id}/runs` and `…/legs` of unknown ids answer 200 `[]` (documented);
  - `/health` `blocks` counts re-read blocks (53 992 against 12 608 rows);
  - Ethereum block `hash` has no `0x`; txids do;
  - the API's `FeeLevel` description still says Bitcoin targets 12 blocks and Solana pays no
    priority fee (`api/wallet_transactions.rs:58`, OpenAPI);
  - `ui/src/money.ts` has dead helpers and a header that says amounts come in display units;
  - the node detail lists every `rpc.` setting except `rps` (`nodes.tsx:212-215`).

---

## Deep dive 3: "Make no-ops visible". Partly fixed

**Done:** each round writes every trigger's last round (time, block, counts, refusal)
(`sdk/trigger/src/round.rs:60-73`, `usecase/workers/trigger/mod.rs:158-180`); a `This {asset}`
sibling not fully indexed refuses the send (`actions.rs:86-91`); an enqueue error becomes the job's
reason (`usecase/workers/index.rs:172-174`).

**Still silent:**
- an empty source or destination returns `Ok` (`usecase/workers/trigger/actions.rs:110-112`); seen
  live: a destination not fully indexed gave `checked: true, jobs: 0, refused: null`
  (`wallet_index.rs:847` leaves out indexes behind the confirmed block; a destination needs only an
  address);
- an amount ≤ 0 or unknown is skipped (`actions.rs:121-123`);
- a compare on something unknown is `false` (`sdk/trigger/src/conditions.rs:118-137`);
- selector `ids` naming missing or paused indexes pick nothing (`sdk/trigger/src/trigger.rs:93-95`,
  `models/wallet_index.rs:784-805`);
- run rows are written only when a trigger acts (`trigger/mod.rs:166-178`); no `trigger.round` log.

**Rules:** empty source or destination → refusal "no source"/"no destination"; unknown amount →
refusal; `ids` naming missing or paused indexes → refusal "not fully indexed"; a destination needs an
address, not a caught-up index.

---

## Cleanup after the QA run

Left in the database (the API has no wallet DELETE):
- `ethereum.sepolia.0x7e5f4552091a69125d5dfcb7b8c2659029395bdf`, imported with the publicly known
  private key `0x…01`: remove by hand;
- `ethereum.sepolia.0x571c8a721f854c3423f39fe07c20fa4b672c442b` (generated key);
- watch-only `ethereum.sepolia.0x000000000000000000000000000000000000dead` and
  `solana.devnet.11111111111111111111111111111112`;
- `qa-send-sol` (57LVrNVWcXxpL3SBEgkdNCUxZ8pGQrbpJxj2CrhbyZsv, empty), `qa-send-eth`
  (0xa27b3cbc6c301edd684ad1266cc519978eb398c6, 0.00002 ETH), `qa-index-devnet-mint` (watch-only).

All qa triggers, tags and indexes were deleted. Backup before the run:
`.data/payment.db.before-fees-20261118-151836`.

The test machine's clock was about 9 hours ahead of chain time, so relative times read "9h ago".

Sources: live probes 2026-11-18 (`api.devnet.solana.com` `getSlot` processed = confirmed =
finalized = 523662574; sepolia node answers for jobs 143703, 143828) · [Solana commitment
levels](https://docs.rs/solana-commitment-config/latest/solana_commitment_config/enum.CommitmentLevel.html) ·
[Infura eth_getLogs limits](https://docs.metamask.io/services/reference/base/json-rpc-methods/eth_getlogs) ·
[geth core/error.go](https://raw.githubusercontent.com/ethereum/go-ethereum/master/core/error.go) ·
[reth pool errors](https://reth.rs/docs/reth_rpc_eth_types/error/enum.RpcPoolError.html).
