# HTTP API

`payment-server` (`apps/server`) is the only process; it serves the routes of the `payment-api`
library (`apps/api`), which names no chain and no storage backend. The OpenAPI 3 contract is
generated from the handlers (Utoipa) and served at `GET /openapi.json`; it is authoritative for
exact schemas. The same document is committed as [`openapi.json`](openapi.json) for typed-client
generation and is regenerated without a server:

```bash
cargo run --locked -p payment-server -- --openapi > docs/openapi.json
```

`apps/api/tests/openapi_contract.rs` fails when the committed copy drifts. Behavioral rules
behind these routes are in `docs/SYSTEM_REQUIREMENTS.md`.

Every asset is an open tag (`btc`, `eth`, `usdc`, `sol`, or whatever the configuration names),
validated against the assets registered at startup: `GET /v1/assets` is the list. There is no
closed chain or asset enum in the wire contract.

## Run

```bash
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- ./payments.toml
```

Storage is one embedded SQLite file, so a run needs no external service. There is no second
backend and nothing selects one: `docs/SYSTEM_REQUIREMENTS.md` puts HA and multi-writer
deployments out of scope, and a single-writer file is what that requires.

The binary takes two other forms, neither of which serves HTTP:

```bash
cargo run --locked -p payment-server -- --openapi          # print the OpenAPI document
cargo run --locked -p payment-server -- ./payments.toml --rescan bitcoin/mainnet
```

`--rescan` is the offline tool of the section below; it adds no route, and `docs/openapi.json` is
unchanged by it.

## Configuration

The document is TOML when the path ends in `.toml` and JSON otherwise; the two decode to the same
value, so every rule below holds for both. Every object denies unknown fields. At least one chain
is required. Secrets (bearer token, database URL, RPC credentials, wallet seeds) are read from named
environment variables, never from the document, and are redacted from diagnostics along with
endpoint text and header values. An RPC endpoint is one URL that describes one node completely; a
literal credential in it is a deployment defect, so it carries a `${NAME}` reference instead.

**Every chain's section has the same shape**: that chain's own keys, one `rpc`, one optional
`cadence`. The name of a section is the name that chain is registered under in
`apps/server/src/chains.rs`; a section no chain is registered for is refused by name, and the
refusal lists the names this build does configure.

```toml
bind = '127.0.0.1:8080'
bearer_token_env = 'PAYMENT_API_TOKEN'
tls_terminated_upstream = false
storage = { path = '/var/lib/payment/payment.db' }

[chains.bitcoin]
network = 'regtest'
confirmation_depth = 6
rpc = { endpoints = ['http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443'] }

[chains.ethereum]
network = 'anvil'
chain_id = 31337
tokens = [{ contract = '0x...', asset = 'usdc', decimals = 6 }]

[chains.ethereum.rpc]
endpoints = [
  { url = 'https://eth.provider.example/v1',
    headers = { 'X-API-Key' = '${ETH_RPC_KEY}' },
    bounds = { timeout_ms = 20000 } },
  'http://127.0.0.1:8545',
]
repeat = { attempts = 3, backoff_ms = 250, max_backoff_ms = 2000 }

[chains.solana]
network = 'local'
genesis_hash = '<base58>'
rpc = { endpoints = [{ url = 'http://127.0.0.1:8899',
                       bounds = { max_request_bytes = 1048576 } }], repeat = 'off' }
tokens = [{ mint = '<base58>', asset = 'usdc', decimals = 6 }]
cadence = { poll_millis = 1000, batch_size = 256 }

[[wallets]]
id = 'treasury-sol'
asset = 'sol'
secret_env = 'TREASURY_SOL_SEED'
start = 0                 # a native block position, or 'fresh'
```

**What changed for an operator** (`docs/design/030` step 11; every existing file needs editing):

| Was | Is | Why |
|---|---|---|
| `chains.<chain>.rpc.retry` | `chains.<chain>.rpc.repeat` | the value and `json_rpc::Retry`, the client decorator it configures, no longer share one word |
| `poll_millis`/`batch_size` beside Bitcoin's and Ethereum's own keys | `chains.<chain>.cadence.{poll_millis, batch_size}` | one section shape on every chain; the flattened form was an accident of who wrote it first |
| `chains.solana.sync` | `chains.solana.cadence` | the key named synchronisation; the thing is a cadence |
| — | `chains.<chain>.cadence.max_backfill_blocks` | optional (default 100000): how far below the checkpoint a new watch may start, which was previously reachable only in code |

| Setting | Rule |
|---|---|
| `bind` | loopback only unless `tls_terminated_upstream` is true |
| `storage` | `{"path": ...}`, the only form (default `payment.db`; a relative path resolves against the working directory). It names a file, never a backend: any other key, including a backend name, is rejected |
| `chains` | the configured chains and their assets; one section per chain, named as that chain is registered (`bitcoin`, `ethereum`, `solana`). Two sections may not name one network |
| `chains.<chain>.asset` | optional asset tag of that network's native wallets (defaults `btc`, `eth`, `sol`). Tags match `[a-z0-9][a-z0-9-]{0,31}` and are unique across the chain's native and token assets |
| `chains.<chain>.rpc.endpoints` | one node per entry, in failover order. Each node carries its own headers and bounds, so two nodes may hold two different API keys |
| endpoint | either the URL alone, or `{"url", "headers", "bounds"}`. Unknown keys are rejected. **The URL is the provider's and reaches the node unaltered** — its path and query are never read as settings and never dropped, so `https://host/v3/<key>` and `?apikey=<key>` are ordinary endpoints. `http`/`https` only, a host is required, a fragment is rejected; `user:password@` becomes HTTP Basic |
| `endpoint.headers` | header name → value, sent on every call to that node |
| `endpoint.bounds` | `timeout_ms` (15000), `max_request_bytes` (10485760), `max_response_bytes` (67108864); each must be > 0 |
| endpoint secrets | a value carries `${NAME}` (`[A-Z_][A-Z0-9_]*`), resolved from the environment at startup, anywhere in the URL and in any header value. A literal credential is a deployment defect. A value resolved into the *URL* may hold only unreserved characters and `+=`: one carrying a delimiter is rejected, because it would reach the node percent-encoded or repoint the URL — put it in a header instead |
| `chains.<chain>.rpc.repeat` | absent = 3 attempts, 250 ms doubling to a 2 s cap, transport failures only. `"off"` = no repetition. Or `{attempts, backoff_ms, max_backoff_ms}` with `backoff_ms <= max_backoff_ms`. One attempt is one complete round over `endpoints`; retry is a property of the list, not of a node, so it is beside the list rather than on a node |
| submission | raw transaction submission is always one attempt against the first endpoint, whatever `repeat` says: the chain states that per call, not the deployment |
| `chains.<chain>.cadence` | optional on every chain, one nested object on every chain: `poll_millis` (1000), `batch_size` (100) and `max_backfill_blocks` (100000), each > 0. The retired `sync` key, the retired `reorg_retention` and a `confirmation_depth` on Ethereum or Solana are all rejected: their nodes state where finality is, so a number there would be a lie |
| `chains.bitcoin.confirmation_depth` | optional, default `6`, accepted range 1..=1000. How many blocks below the node's tip this deployment commits, because Bitcoin has no finality and something must choose. **The trade-off:** a smaller depth makes history and spendable balance appear sooner — six is roughly an hour, one is roughly ten minutes — and a reorg deeper than it quarantines that network with a terminal `confirmed_reorg` instead of repairing itself, leaving the offline rescan (`--rescan <chain>/<network>`, below) as the only remedy. Six is the industry's settlement depth and is deeper than any Bitcoin mainnet reorg since the March 2013 consensus bug (24 blocks, a software defect rather than competing work); mainnet has seen no reorg of three blocks or more since. A hundred reproduces the old rollback journal's safety envelope exactly, at roughly sixteen hours of latency. The startup log states the line this deployment runs at, and `GET /v1/status` reports it continuously as `checkpoint` against `pending_block` |
| `ethereum` node requirement | every configured endpoint must serve `debug_traceBlockByHash` with the `prestateTracer` in `diffMode`; startup traces the node's own head and refuses it otherwise, naming the method. Contract-initiated ether is invisible without it, so there is no flag to disable the check. It is a **startup** refusal, not a quarantine: every chain's identity is verified before storage opens, so a node that fails the probe stops the process before any chain runs. Quarantine is an *indexing* failure, after startup, and does isolate one chain |
| `ethereum.limits` | optional caps: input bytes, gas margin/limit, per-gas fees, total fee |
| `ethereum.tokens` | allowlists ERC-20 contracts, each `{contract, asset, decimals}`: canonical lowercase non-zero contract, its own asset tag, decimals 0-36. Startup reads each contract and rejects a mismatch. A non-empty list requires exactly one Ethereum endpoint |
| `solana` | exactly one element in `endpoints`; every Solana call is one attempt, so `repeat = 'off'` only restates it. Genesis hash verified at startup; no commitment, fee, lag, or Memo options |
| `solana.tokens` | allowlists SPL mints, each `{mint, asset, decimals}`: canonical Base58 mint (not the System program address), its own asset tag, decimals 0-19. Startup reads each mint account and rejects a mismatch, a non-mint, or a Token-2022 mint (its confidential transfers need not report an amount). Token wallets receive, report balances, and serve history; a send from one is refused rather than moving SOL under the token's tag |
| `wallets[].start` | the birthday to index from: a native block position (`0` is genesis), or `'fresh'` for a key that has never been used, which starts at the checkpoint successor. The same vocabulary the HTTP add paths use. The retired `start_position` and `start_height` are both rejected, and the field has no default |
| `wallets[].asset` | a configured asset tag; an unknown tag fails startup |
| `wallets[].secret_env` | 64 hexadecimal characters, either case, on every chain: one 32-byte key, decoded by `crypto::SecretBytes::from_hex` and checked for length and curve by the asset's factory |

## Authentication and health

- Wallet and transaction routes require `Authorization: Bearer <token>`.
- `GET /health/live`, `GET /health/ready`, and `GET /openapi.json` are public. Health returns `204`;
  ready returns `503` when wallets or indexes are not ready.
- Ready means every configured network is `ready` or `quarantined`, and at least one is `ready`. A
  quarantined network does not hold readiness at `503` forever - the healthy chains still serve,
  and the dead one is named in `GET /v1/status`. Every network quarantined answers `503`, and the
  process still does not exit.
- Readiness follows **forward** indexing only. A network that reached its tip is `ready` even
  while it still owes history below its checkpoint for an address watched from before it was
  registered; that range is reported as `networks[].backfill` on `GET /v1/status`, and every read
  it weakens carries its own `completeness`. A historical range a node can never serve therefore
  costs those addresses their old history and nothing else.
- The listener binds only after every network has settled: reached Ready with a persisted
  checkpoint at least once, or become quarantined.

## Routes

| Method | Path | Success | Abstraction | Notes |
|---|---|---|---|---|
| `GET` | `/v1/assets` | `200` | `Wallets::assets` | `{"assets":[{"asset","network":{"chain","name"},"metadata"}]}`: every registered tag |
| `GET` | `/v1/assets/{asset}/address?text=...` | `200` | `Wallets::parse` | canonical `{network, value}` of `text`; `400` when the asset's chain rejects it. Address text is arbitrary caller input, so it is a query parameter, never a path segment |
| `POST` | `/v1/wallets` | `201` wallet | `Wallets::create` + `Watching::watch` + `Signers::keep` | body `{"asset":"btc","start":"fresh"}`; the key is generated server-side and held **in memory only** (see Key custody). **Not idempotent**: a retry mints a second wallet and a second key (see below) |
| `POST` | `/v1/wallets/import` | `201` wallet | `SecretBytes::from_hex` + `Wallets::from` + `Watching::watch` + `Signers::keep` | body `{"asset":"btc","secret":"<key text>","start":842000}`; indexed from `start`, whose history below the checkpoint is backfilled |
| `POST` | `/v1/addresses` | `201` wallet | `Wallets::parse` + `Watching::watch` | body `{"asset":"btc","address":"<text>","start":"fresh"}`; watch-only entry (`watch_only: true`) |
| `GET` | `/v1/wallets` | `200` | `Watching::watched` ∪ `Signers` | `{"wallets":[...]}` in ID order, wallets and watched addresses |
| `GET` | `/v1/wallets/{id}` | `200` wallet | `Watching::watched` ∪ `Signers` | `{id, asset, network: {chain, name}, address, watch_only}`; never secrets |
| `GET` | `/v1/wallets/{id}/balance` | `200` | `indexing::Balances::balance`, folded over the movements the index holds *and* the value its blocks moved with no transaction to name it | `{"amount":"12.5","pending_incoming":"3.0","pending_outgoing":"0","observed_height":42,"pending_height":48,"completeness":"incomplete"}`. A watched address answers it too: the fold needs no key, and `observed_height` is non-null on every chain once that chain has committed a block. On Ethereum the amount includes consensus-layer withdrawals, which the block body credits directly with no transaction — so a validator withdrawal address can report a balance while `GET /v1/wallets/{id}/transactions` is empty, because that page lists transactions and a withdrawal is not one |
| `GET` | `/v1/wallets/{id}/transactions` | `200` page | `History::history` | `limit` 1..=1000 (default 100), opaque `cursor`; works for watched addresses |
| `POST` | `/v1/wallets/{id}/transactions` | `202` `TransferAccepted` | `Wallets::parse` + `Wallets::author`, then the author's own `send` | body `Transfer`; `422` for a watched address, `400` for an asset this deployment only receives |
| `POST` | `/v1/transactions` | `202` `BatchAccepted` | `Wallets::send_all` | body `TransferBatch` of `BatchedTransfer`; `400` with no `failed_index` for an asset this deployment only receives |
| `GET` | `/v1/status` | `200` | `SyncHealth` + `Checkpoint` | `{"ready":true,"networks":[{"network":{"chain","name"},"checkpoint","pending_block","state","failure","backfill"}]}` |
| `POST` | `/v1/status/resume` | `202` `ResumeAccepted` | `indexing::SyncControl::resume` | body `{"network":{"chain":"bitcoin","name":"mainnet"}}`; clears that one network's quarantine. `404` for a network this deployment does not configure, `409` for one that is not quarantined |
| `POST` | `/v1/status/backfill/cancel` | `202` `BackfillCancelled` | `indexing::Backfilling::cancel` | body `{"network":{"chain":"bitcoin","name":"mainnet"}}`; stops asking for the range `networks[].backfill` reports. `404` for a network this deployment does not configure, `409` for one that owes no history |
| `GET` | `/health/live`, `/health/ready`, `/openapi.json` | `204`/`204`/`200` | `SyncHealth` readiness | public, no bearer token |

- The chain and network of a wallet, address, or transaction always ship as one nested
  `network: {chain, name}` object; they are never two sibling fields.
- Network and token contract come from startup configuration, never from requests.
- **`POST /v1/wallets` is not idempotent.** There is no request key and no deduplication: a
  retried or replayed create mints a second wallet with a second generated key, and the first
  wallet keeps existing and keeps being indexed. Clients that retry must treat the returned `id`
  as the only record of the attempt, or reconcile with `GET /v1/wallets`. The same holds for
  `POST /v1/wallets/import` and `POST /v1/addresses`, except that both answer `409` when the
  address is already registered.
- A generated, imported, or watched address is watched by the index before `201`, and that
  durable row is committed *before* the key is filed in memory: a crash between the two leaves a
  watch-only address whose history is correct, never a key nothing observes.
- **Every add path states a birthday.** `start` is required on `POST /v1/wallets`,
  `POST /v1/wallets/import` and `POST /v1/addresses`, and has no default: an omitted one is a
  `400` from the parser, because a birthday nobody stated was once accepted silently and indexed
  nothing. It is either the string `"fresh"` — this key or address has never been used, so
  observation begins at the block after the current checkpoint — or a **native block position**
  (a height on Bitcoin and Ethereum, a slot on Solana). `0` is genesis, which is why the sentinel
  cannot be a number.

  A position below the checkpoint is **backfilled**, not discarded: the index fills that range in
  below the checkpoint while forward indexing continues, and every read of that address reports
  `completeness: "incomplete"` until it is done. A position further below the checkpoint than
  `chains.<chain>.cadence.max_backfill_blocks` allows is refused by this call with `400`, naming
  the earliest start it would accept — synchronously, so a mistyped birthday never becomes a
  background failure an hour later.
- **Key custody, and what a restart brings back.** A key generated or imported through the API
  lives in this process's memory for as long as the process does, and **nowhere else**. The index
  stores no key material: its rows say what is observed, from which block, and in which asset
  (`docs/design/030 §8 (ex-029)`). Keys never appear in responses, logs, or `Debug`.

  A restart therefore brings back two different halves from two different owners. Every watched
  address comes back from the index, with its id, asset, network, address and birthday, and its
  history keeps working. Whether it can *sign* comes back only from key material the application
  supplies at startup — the `wallets[]` entries, each naming an environment variable — so an id
  created through `POST /v1/wallets` or `POST /v1/wallets/import` reads back with
  `watch_only: true` after a restart, and a send from it answers `422`. That is the honest
  failure: the address is still observed and its history is still correct, and re-importing the
  key repairs it. **Keep the key you send to `POST /v1/wallets/import`, and treat
  `POST /v1/wallets` as development-only** unless you add the generated key to `wallets[]`
  yourself. This holds for **every** chain, Solana included: no chain is exempt
  (`docs/SYSTEM_REQUIREMENTS.md` §Key custody).

- A watch-only address (`POST /v1/addresses`) stores no key: it has history and appears in
  filters, answers balance and history like any wallet, and answers `422` for sends only.
- A token wallet's balance and history are that token only; the native coin needed for gas is
  checked on send and shown only as fee metadata.
- `202` means submitted, not confirmed. Read history for inclusion, confirmation, and reorgs.

### Synchronization status

`GET /v1/status` reports one row per configured network, in index order. Each row carries the
network's last committed `checkpoint` (read from the index, so it is current), the `state` its
synchronization is in, `failure` - present exactly when the state is `retrying` or
`quarantined`, `null` otherwise, and always present as a field - and `backfill`, the history that
network still owes addresses watched from below its checkpoint.

| `state` | Meaning |
|---|---|
| `catching_up` | behind the node's tip and still applying blocks. `GET /health/ready` answers `503` until it catches up |
| `ready` | the checkpoint is at the tip the last pass observed. A `ready` network may still owe history below its checkpoint; see `backfill` |
| `retrying` | the last pass failed retryably and will be attempted again, with growing backoff |
| `quarantined` | the last pass failed terminally; this network alone stops being indexed, keeps its last checkpoint, and keeps answering history from it. Other networks are unaffected |

```json
{
  "ready": true,
  "networks": [
    { "network": {"chain": "ethereum", "name": "mainnet"},
      "checkpoint": {"height": 20310441}, "state": "ready", "failure": null,
      "backfill": { "pending": 1, "resume": 18000000, "anchor": 20310002,
                    "failure": { "kind": "source", "retryable": true, "attempts": 12,
                                 "since": 1758614400,
                                 "message": "block is not available on this node" } } },
    { "network": {"chain": "bitcoin", "name": "mainnet"},
      "checkpoint": {"height": 845112}, "state": "quarantined", "backfill": null,
      "failure": { "kind": "invalid_block", "retryable": false, "attempts": 1,
                   "since": 1758614400,
                   "message": "block contains a duplicate transaction" } }
  ]
}
```

**`backfill` is reported, never gating.** The row above is a network whose node has pruned the
range one watch asked for: forward indexing is at the tip and every client read is answered, so
the network is `ready` and `GET /health/ready` answers `204`. Only the addresses inside
`[resume, anchor]` are short, and every balance and history page for them says so through its own
`completeness`. The alternative - holding the whole API at `503` until a range a pruned node will
*never* serve closes - took every chain down for one watch, indefinitely, and is what
`docs/design/024` §7 calls the availability floor.

`backfill.failure` repeating with a growing `attempts` is the signal that the range is unservable
rather than merely slow. The remedy is a node that holds the range, or a watch whose birthday is
one this node can serve; the index retries with the same capped, jittered backoff a failed pass
gets, so a permanently pruned range costs a bounded trickle of calls rather than a spin.

### Stopping a range nobody will serve: the cancel

Reporting bounds the cost; it does not end it. Nothing in the index ever gives up on a range, so a
watch whose birthday this node cannot serve keeps one refused fetch per pass for the life of the
process. One request stops asking:

```bash
curl -X POST http://127.0.0.1:8080/v1/status/backfill/cancel \
  -H "Authorization: Bearer $PAYMENT_API_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"network": {"chain": "bitcoin", "name": "mainnet"}}'
```

```json
{ "network": {"chain": "bitcoin", "name": "mainnet"},
  "watches": 1,
  "stopped": { "pending": 1, "resume": 18000000, "anchor": 20310002,
               "failure": { "kind": "source", "retryable": true, "attempts": 37,
                            "since": 1758614400,
                            "message": "block is not available on this node" } } }
```

- Every watched address that still owed history is left as one **created now** would be: same
  `id`, same address, birthday at the checkpoint's successor, nothing inspected. `GET /v1/wallets/{id}`
  answers exactly as before; this route is not an unwatch.
- **Nothing indexed is deleted.** Rows a partial backfill already wrote, and rows the address
  collected as somebody else's counterparty, stay and are still served. What the raised birthday
  stops is the *claim*: a read that would have said `incomplete` about the original birthday now
  says it about the new one, which is the only honest thing to say once the range below will not be
  read. Forward indexing is untouched and pauses for nothing.
- It names **one** network, for the reason the resume does and for one more: `networks[].backfill`
  is a network aggregate that names no address, so a per-address cancel would be a request this API
  gives nobody the information to make. `watches` is what the index actually re-anchored and
  `stopped` is what `GET /v1/status` held immediately before; they differ by whatever a pass
  settled in between.
- **The range can be asked for again**, at the price of reading it: watch the address from a lower
  `start`, or purge the recorded progress with the offline rescan below, which re-derives every range
  from its registered birthday. Cancelling drops the claim, never the ability.
- `404` names a network this deployment does not configure; `409` one that owes no history, because
  reporting an action that did not happen is worse than refusing.

`failure.kind` is the indexing error category (`source`, `store`, `conflict`, `network_mismatch`,
`invalid_block`, `cannot_connect`, `confirmed_reorg`, `invalid_request`), `attempts` is the count of
consecutive attempts that got nowhere — failed passes, and passes whose backfill was refused — and
`since` is the Unix second the network entered `state`. A
quarantined network is never retried automatically, because the classes that reach it are
deterministic; clearing it is an operator action taken once its cause is fixed.

### Clearing a quarantine: the resume

Once the cause is fixed out of band -- the node upgraded, the store repaired, the configuration
corrected -- one request puts that network back to work, without restarting the process and
without touching any other chain:

```bash
curl -X POST http://127.0.0.1:8080/v1/status/resume \
  -H "Authorization: Bearer $PAYMENT_API_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"network": {"chain": "bitcoin", "name": "mainnet"}}'
```

```json
{ "network": {"chain": "bitcoin", "name": "mainnet"},
  "cleared": { "kind": "invalid_block", "retryable": false, "attempts": 1,
               "since": 1758614400,
               "message": "block contains a duplicate transaction" } }
```

- It names **one** network, spelled exactly as `GET /v1/status` spells it. There is no "resume
  everything": which chain was repaired is the fact the operator has and the server does not.
- `202`, not `200`: the attempt count resets and the next pass runs at once, and that pass decides.
  Nothing durable changes. `cleared` is what `GET /v1/status` reported for the network immediately
  before, not a claim that the cause is gone -- a network whose cause is still unfixed fails again
  on that pass, through the ordinary `retrying`/`quarantined` path, and says so on `GET /v1/status`.
  Read `GET /v1/status` after a resume; do not read the `202` as health.
- `404` names a network this deployment does not configure; `409` a network that is not
  quarantined, because reporting an action that did not happen is worse than refusing.
- A quarantine covers the whole synchronization unit that owns the network, so a resume covers
  every network that shares that unit. Today each chain is its own unit.
- It cannot fix `confirmed_reorg`, which needs the offline rescan below: resuming it re-reads the
  same block and quarantines again -- visibly, and at the ordinary backoff, which is the point.

### Clearing a quarantine: the offline rescan

Most quarantines clear once their cause is fixed out of band -- a node upgraded, a configuration
corrected -- and the resume above puts the network back to work. One does not: `confirmed_reorg`
means the
chain took back a block this deployment had already committed as irreversible, so a restart
re-reads the same block and quarantines again. It is Bitcoin-only (a reorg deeper than
`chains.bitcoin.confirmation_depth`) and the only remedy is to re-index that network from scratch:

```bash
payment-server ./payments.toml --rescan bitcoin/mainnet
```

- **Stop the server first.** The subcommand binds no listener and starts no indexing, but the
  running server does. Purging under a live API would serve an empty history that looks
  authoritative and would break checkpoint-bound history cursors mid-page.
- It removes that network's checkpoint, history, movements, and live outputs -- and nothing else.
  Watched addresses and their birthdays live in the registry, which it never touches, and every
  other configured network is untouched.
- It says what it is about to do, prints the row counts it removed, and exits non-zero on failure.
- It refuses a malformed argument, and a network this configuration file does not configure,
  before opening storage.
- The next ordinary start re-indexes the network from the earliest registered address start
  position. Expect a full catch-up: `GET /v1/status` reports `catching_up` until it returns to
  the tip, and history for that network is incomplete while it does.

### History page

```json
{
  "checkpoint": { "position": 42, "height": 42, "hash": "<hex>", "parent": { "position": 41, "hash": "<hex>" } },
  "transactions": [],
  "pending": [],
  "next_cursor": null,
  "completeness": "incomplete"
}
```

A page is the wallet's history in the wallet's asset (`indexing::History::history` of the wallet itself, one `indexing::Page` at a time, answered by `indexing::ObservedHistory` over the repository's own query and its index's overlay; the selection contract is on `indexing::HistoryQuery`):
every transaction that moves that asset to or from the wallet's address, every failed transaction
the address paid the fee for, and, for a native-asset wallet, every transaction the address paid
the fee for. Each transaction has its `network`, `transaction_id`, status (inclusion or
confirmation at a block), its movements of the wallet's asset, and an optional fee (always the
chain's native asset; a page that prices one in anything else is refused whole, with `503`, before
any of it is converted). Amounts are exact decimals in display units: the index answers in atomic
units and never learns an asset's decimals, so every movement, every fee and every balance is
scaled here by the unit its own `asset` resolves to — the unit each chain's configuration declares
(its own currency's, and each allowlisted token's, in the scale startup checked the contract
against). Every `Option` the server always writes stays `required` in the schema and
is `null` when absent; only `transaction_ids`, `failed_index`, and `ambiguous_transaction_id` in
an error body are omitted entirely.

`checkpoint` is the network's **confirmed line**. `transactions` holds what is at or below it:
committed, append-only, and never reordered or removed, with `status.kind` `confirmed` or
`failed`. It is ordered by block and then by the transaction's own place **inside** that block —
the chain's execution order, which each chain reports and the index stores rather than derives
(Ethereum's `transactionIndex`, the position in Bitcoin's `tx` array and in Solana's
`transactions` array). Both components are facts about the block rather than about the order the
index learned them in, so re-reading a stored block for a newly watched address leaves every
transaction already on a page exactly where it was — which is what the cursor promise below
rests on. `pending` holds the same wallet's transactions in canonical blocks *above* the line,
oldest first, with `status.kind` `pending`; they can still revert, which is why they are not in
`transactions`. `pending` is populated on the first page only (`cursor` absent) and is bounded by
the chain's confirmation depth — it is empty on Ethereum and Solana whenever the node's own
finality has caught up.

A page holds at most `limit` confirmed items and is full unless it is the last; `next_cursor` is
null exactly when no further transaction matches. The cursor carries a position and nothing else:
confirmed history is append-only, so a checkpoint that moved can only have appended *above* the
cursor, and a cursor stays valid however long a caller holds it. There is no `409` on paging.

### How complete an answer is

`GET /v1/wallets/{id}/balance` and `GET /v1/wallets/{id}/transactions` both carry `completeness`,
always present and spelled the same way in both places. It is a property of the **read**, not of
the wallet: `overdrawn` is a fact about one fold and `incomplete` a claim about one start, and
one per-wallet state would have to pick a single grain for both.

**Both reads answer it from the same fact**, which is the observation's own `start` and how far
indexing has read for it - so a page and a balance of one wallet cannot disagree about how far back
the answer reaches. `overdrawn` is the one value only the balance gives: it is what the arithmetic
proved about a total, and a page performs no arithmetic. On a history page the claim is about the
*stream*, not the page in hand - `complete_history` means walking every page lists every
transaction the address ever took part in, which is why `next_cursor` being `null` is not the same
statement and cannot stand in for it.

**Two of these values were renamed**, because one name covered two different claims. `complete`
said the start was proven early enough, while the only thing that ever produced it was a node read
agreeing with the fold at one block - which proves the *total* there and says nothing about the
start. The two claims are now named apart, and the name that over-claimed is retired.

| Value | Meaning | Was |
|---|---|---|
| `incomplete` | every change since the watch's `start` is indexed, and nothing proves the start was early enough. The ordinary answer, and the only one most assets ever give | `from_birthday` |
| `verified_opening_balance` | the settled total was checked against an independent node read of the same holding pinned to the same block, and the two agreed. It needs a chain that can pin such a read; only Ethereum's balance witness does that today, so Bitcoin and Solana never report it. Evidence about the **total**, not about how far back observation reached | `complete` |
| `complete_history` | every change the address ever made is indexed, because observation began at or before its first use. Produced in exactly one case: the watch's `start` is the chain's first block and indexing has reached it, so no earlier block could hide a credit. For a `start` above the first block the index refuses to pass it on - so a store that claims it without having earned it cannot reach a caller | - |
| `overdrawn` | the fold went negative, which a complete index cannot do: more left the address than ever reached it, so credits are provably missing and `amount` is `"0"`, because no honest number exists | unchanged |

A read of an address whose historical range is still arriving is weakened to `incomplete`
whatever the fold would otherwise have claimed - `overdrawn` survives, because it is a proof and a
backfill only ever adds credits. On the history page this is the one thing `next_cursor` cannot
say: the cursor speaks about what follows the page, and a backfill inserts rows *before* it, so a
caller paging to the end mid-backfill would otherwise hold what reads as a complete export and is
not one.

### Send bodies

All bodies are exact schemas (`additionalProperties: false`); any unknown member at any level
returns `400` `request body must match the documented JSON schema` before any wallet call.

```json
{ "destination": "<address>", "amount": "1.25" }
```

```json
{ "transfers": [ { "wallet_id": "019...", "destination": "<address>", "amount": "2.5" } ] }
```

- The single body is `Transfer` and answers `TransferAccepted`; the batch body is `TransferBatch`
  of `BatchedTransfer` and answers `BatchAccepted`.
- `destination` is a plain string: the address text as the wallet's chain writes it. The
  wallet's chain parses it for the wallet's network and uses its canonical form: Ethereum `0x` hex
  in any case (mixed case must carry a valid EIP-55 checksum), Bitcoin text of the wallet's network
  in any case, Solana canonical Base58. Invalid text returns `400` (with `failed_index` in a batch)
  before any transaction is prepared. `amount` and `wallet_id` are strings. There is no object
  destination form and no `encoding` member; the address text alone determines its encoding.
- Wallet responses are unchanged: `address` is the canonical text (Ethereum lowercase `0x` hex).
  History addresses are `{network, value}` with the same canonical `value`.
- `transfers` has 1..=50 items (`minItems`/`maxItems`, no `uniqueItems`). Empty -> `400`
  `at least one transfer is required`; over 50 -> `400` `at most 50 transfers are allowed`; neither
  carries a failed index.
- Any non-empty query string on either POST returns `400`
  `transaction query parameters are not supported`. No header controls send behavior.
- Precedence: transport/auth, query rejection, JSON schema, batch cardinality, item conversion
  (amount text, then the destination parsed by the item's wallet, which also rejects an unknown
  wallet), per-item validation (amount, wallet, asset, destination network) in order, chain
  preparation, ordered broadcast.

## Errors

Body: required `message`; optional `transaction_ids` (acknowledged prefix), `failed_index` (only
when one original item truthfully failed), `ambiguous_transaction_id` (locally derived; always
`503`).

```json
{
  "message": "Ethereum submission outcome is ambiguous",
  "transaction_ids": ["<accepted id>"],
  "failed_index": 1,
  "ambiguous_transaction_id": "<locally derived id>"
}
```

Every rejection uses this body and `Content-Type: application/json`, including the ones the
framework produces before a handler runs: an unknown or malformed query parameter, a malformed
path parameter, a missing or wrong bearer token (`401`), and a body sent without the JSON content
type.

| Status | Meaning |
|---|---|
| `400` | malformed request, query, path parameter, cursor, or batch size, or a send of an asset this deployment only receives (it declares no `wallets::Authorship`, so neither route has a sender to reach) |
| `401` | missing or invalid bearer token |
| `404` | asset not configured or wallet unknown |
| `422` | deterministic transaction rejection, or a watch-only address asked to sign |
| `409` | conflicting state (a duplicate wallet id or address); history paging no longer produces one |
| `503` | wallet/index unavailable, source busy, or ambiguous submission |
| `500` | response could not be encoded |

Clients MUST NOT automatically retry an ambiguous or unknown send: the API has no idempotency
store, and a new request can pay twice.
