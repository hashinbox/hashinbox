# 038: The simple server — custody, healthz, sweep

> "api server has simple purposes and we can simplify it ... its goal is to
> receive funds and count how much received for each wallet, then queue up
> several transfers from each wallet ... most of the logic should be in
> apps/server, with huge help of sdk."

Status: **proposal.** This document narrows the product surface to one
purpose: a custodial wallet server. Wallets are created server-side, their
keys persist in the same SQLite file that indexes them, funds are received
and counted per wallet, and funds go out by transfer or by sweep. It
supersedes the custody closure in `017` section 2 and shrinks the route
surface of `035`/`037`. Open decisions are collected in §7 with defaults;
flip any before work starts.

---

## 0. The direction

- Create a wallet, get its address back. Creation is the birthday.
- The private key persists in the same SQLite file that serves the index.
- The API never reveals a key.
- Read a wallet's balance: confirmed and pending.
- Read a wallet's full transaction history.
- Send funds out. Queue several transfers per wallet. Sweep: spend
  everything, e.g. all of a wallet's UTXOs to one address.
- `healthz` from memory: 200 when everything is in order, plus per-RPC
  failure statistics.

---

## 1. What disappears

- **`GET /v1/assets/{asset}/address`.** The parse convenience. No consumer
  exists; watch and send parse internally.
- **`POST /v1/wallets/import`.** The scope is create-and-watch. If a crash
  order ever strands a key, import returns as the repair — until then it is
  out.
- **`GET /v1/status`, `POST /v1/status/resume`, `POST /v1/status/backfill/cancel`.**
  Operator actions, not product surface. Their information folds into
  `healthz`'s body. Resume and backfill-cancel become operator CLI
  subcommands beside `--rescan`.
- **The `start` birthday parameter.** Creation is the birthday. `CreateWallet`
  and `WatchAddress` bodies shrink to what they name.

## 2. The surface

| Route | What it does |
|---|---|
| `GET /healthz` | §4. 200 from memory when in order; per-RPC stats in the body. |
| `GET /v1/assets` | Every configured asset tag. Kept: callers need it. |
| `POST /v1/wallets` | Generate a key, persist it, watch it, return id + address. 201. |
| `GET /v1/wallets` | Every wallet and watched address, in id order. |
| `GET /v1/wallets/{id}` | Public metadata: address, asset, `watch_only`. |
| `GET /v1/wallets/{id}/balance` | Confirmed + pending, display units. |
| `GET /v1/wallets/{id}/transactions` | Full history, cursor-paged, pending on the first page. |
| `POST /v1/wallets/{id}/transactions` | One transfer out. 202. `amount: "all"` sweeps (§5). |
| `POST /v1/transactions` | Ordered batch of transfers across wallets. 202. |
| `POST /v1/addresses` | Watch an address without its key. 201. |

Nine resources, all nouns. The verb routes of the `035`/`037` reviews
disappear with the cut rather than being renamed.

---

## 3. Custody: the wallet registry

Today `Signers` is an in-memory map (`apps/server/src/api/signers.rs:15`);
keys die with the process. This document makes the server the custodian.

- **One table in the same SQLite file.** `wallet(id, tag, secret, …)`,
  owned by `apps/server`. Application-owned, exactly as the rescan rules
  already require: `purge_network` never touches it, unrelated networks are
  untouched.
- **Boot.** Open the registry, reconstruct each wallet through its asset's
  factory, fill `Signers`. Everything after boot is unchanged.
- **Creation order flips.** Today's rule is watch-first
  (`docs/design/030 §8 (ex-029)` section 3.3): a crash leaves a watch-only
  address, repairable by re-import. With import out of the product, a
  watch-only wallet is unspendable garbage. New order: **registry first,
  watch second.** A crash between them leaves a stored key nothing watches;
  the caller got a 500 and no address, so no funds ever reach it. Orphan
  keys are inert and can be listed by an operator command.
- **Never reveal.** No endpoint returns `secret`. The registry is read at
  boot and never serialized. Memory hygiene stays as the workspace rules
  already demand (zeroizing secret memory; no secrets in `Debug`, logs, or
  errors).
- **Encryption at rest: D1.** Default: the stored secret is encrypted under
  a passphrase the config supplies (environment-sourced). Plaintext storage
  is refused unless config explicitly allows it. The table schema does not
  care; the codec does.

## 4. healthz and per-RPC statistics

- **One endpoint.** `GET /healthz`, public. The process answering is the
  liveness signal; the readiness rule is the existing one — every network
  settled (`ready` or `quarantined`) and at least one `ready`
  (`indexing::SyncHealth::ready`), read from the health channel in memory.
  200 when ready, 503 otherwise. No RPC is made to answer it.
- **Stats in the body.** Per network: state, checkpoint, pending tip,
  failure, owed backfill — what `GET /v1/status` carries today. Per RPC: a
  failure counter per chain client — attempts, failures, consecutive
  failures, last failure message — "which rpc is failing or not."
- **Where counters live.** The `json-rpc` transport already sees every
  call's outcome. A small stats handle there, wired by the composition root
  into the health snapshot, is the whole mechanism. Counters are atomics;
  the endpoint stays allocation-light and never blocks.

## 5. Sweep

"Send all BTC inputs to one wallet" is the named case. Nothing like it
exists today: `Wallets::send_all` is batch orchestration, not spend-all.

- **SDK: a drain mode.** The bitcoin builder gains a sweep construction:
  every live UTXO of the wallet is the input set; the amount is
  `Σ inputs − fee`; one destination; no change (change to self only when a
  protocol rule requires an output). Account chains get the same verb as
  `balance − fee`, one nonce, one destination. The SDK owns the mechanics
  because they are chain invariants.
- **apps/server: the orchestration.** A sweep is a server flow, not a
  single builder call: list the wallet's live outputs through the existing
  output reader, ask the wallet for the sweep transaction, sign, submit
  through the existing outbox path. The route surface stays one: the
  transfer body accepts `amount: "all"`, and the server takes the sweep
  path for that item — single and batch alike.
- **Refusals.** Sweep of a watch-only wallet: 422, as any send. Sweep with
  no live funds: 400 with that said. Fee bounds still apply.

## 6. Counting received

No new endpoint. "How much did this wallet receive" is the history page's
credits folded over, and the balance is its current residue. The page and
the fold already answer both; the product reads them.

---

## 7. Open decisions

| # | Question | Default in this document |
|---|---|---|
| D1 | Keys at rest | Encrypted under a config/env passphrase; plaintext only by explicit config |
| D2 | Wallet import | Cut; returns as the crash-repair tool if ever needed |
| D3 | Watch-only addresses | Kept — "we create wallets and we watch them" |
| D4 | healthz shape | One endpoint serving liveness + readiness + stats |
| D5 | Sweep trigger | `amount: "all"` on the existing transfer body, single and batch |

## 8. What this supersedes

- `017` section 2's custody closure ("custody is the API caller's problem")
  is reversed: the server is the custodian. `docs/SYSTEM_REQUIREMENTS.md`
  and the custody section of `docs/API.md` rewrite in this document's
  change set.
- `035`/`037` reviews proposed renaming the `status` routes to nouns; the
  cut makes that moot.
- The watch-first creation order (`030 (ex-029)` section 3.3) is replaced
  by registry-first, with the reason recorded there.

## 9. Work order

One step per change set. Every step leaves the workspace green.

1. **The registry.** Schema step for the `wallet` table (versioned, in the
   existing migration chain). Custody codec (D1). Boot reconstruction.
   Creation flips to registry-first. `Signers` becomes the in-memory index
   it already is, fed from the registry instead of only from requests.
2. **The cut.** Remove the five routes and the `start` parameter.
   `docs/openapi.json`, the route contract tests, and `docs/API.md` shrink
   together. Resume and backfill-cancel land as operator CLI subcommands.
3. **healthz.** The stats handle in `json-rpc`; the health snapshot merges
   network health and RPC counters; `/healthz` serves it.
4. **Sweep.** SDK drain mode (bitcoin first; account chains as
   balance-minus-fee). The `"all"` amount path in the server, single and
   batch.
5. **Docs.** `SYSTEM_REQUIREMENTS.md`, `API.md`, the server README's run
   section, and the custody reversal note in `017`'s successors.

## 10. Out of scope

- **No multi-sig, no HSM, no remote custody.** One server, one SQLite file,
  keys it can unlock.
- **No withdrawal policy engine.** Sweep and batch send what the caller
  asks; limits are the existing fee bounds and batch size.
- **No new query surface.** Received-counting stays reads of the existing
  page and fold.
- **No changes to indexing.** The registry is application data beside the
  repository, not a new indexing concern; the ports do not grow.

## 11. Verification

Per step: the eight gates from `AGENTS.md`, plus the Linter MCP `check`.

Behavioral proof for the load-bearing claims:

- **Crash order.** A failure between registry insert and watch commit
  leaves a stored key no route ever served; a test drives the abort and
  asserts the wallet is absent from every read.
- **Boot.** A created wallet survives a restart: same id, same address,
  signs a transfer. A test restarts the composition over one file.
- **Never revealed.** The contract test that no response schema carries a
  secret field extends to the registry-era responses.
- **healthz.** 200 on a healthy composition, 503 with a quarantined
  network; the body carries a failing RPC's counter after a fault is
  injected.
- **Sweep.** End-to-end on the RPC double: all UTXOs in, one destination,
  amount equals inputs minus the fee the double pins; sweep of a watch-only
  wallet is 422; sweep with no live outputs is 400.
