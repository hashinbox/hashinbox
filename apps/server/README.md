# payment-server

The one executable of the workspace: it reads a configuration file, builds
`payments::Payments` over one SQLite file with the Bitcoin, Ethereum and Solana
chains registered, runs every network's indexer and the send queue, and serves
the wallet HTTP API. It is a binary with no library.

## Usage

```bash
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- migrate ./payments.toml  # once: create the database
cargo run --locked -p payment-server -- serve ./payments.toml
```

`migrate` executes `schema/sdk.sql`, `schema/server.sql` and `schema/queue.sql`
on the configured (new) file; there are no migrations. `serve` only opens it.

```toml
# payments.toml (a `.toml` path is read as TOML, anything else as JSON)
bind = '127.0.0.1:8080'
bearer_token_env = 'PAYMENT_API_TOKEN'   # the variable that holds the API token
storage = { path = 'payment.db' }        # optional, this is the default

[[payments.networks]]
chain = 'bitcoin'
network = 'regtest'
endpoints = ['http://user:password@127.0.0.1:18443']

[[payments.networks]]
chain = 'ethereum'
network = 'mainnet'
chain_id = 1
endpoints = ['https://node.invalid/v3/KEY']
```

`payments` is `payments::Document`: one section per network, its `chain` naming
the chain `src/main.rs` registers; the rest is that chain's own `Config` (see the
chain's README). Every network is built at startup; one that fails verification
stops the start. `bearer_token_env` is optional only for a loopback `bind`.

A network's stored `rpc_endpoints` row replaces its section's `endpoints` at
startup (matched on the section's `chain` and `network` text).
`PUT /v1/networks/{chain}/{network}/rpc` stores a list and moves every later call
of the network to it at once (`rpc::Failover` behind the network's
`rpc::Swap`), unverified; the chain verifies it on the next start.

The server also serves a browser page at `/` (`[ui] brand`, `PAYMENT_UI_API_BASE`).
The committed page (`ui/dist`) was built against the previous API and needs a
rebuild for the routes below.

## API

`payment-server --openapi` prints the OpenAPI document; `docs/openapi.json` is the
committed copy. `GET /health` and `GET /openapi.json` need no token.

- `GET /v1/assets`: every configured asset (`payments.assets()`): its network, its
  id (`native` or a token's) and metadata.
- `POST /v1/wallets` `{chain, network, secret?, address?}`: with `secret` (a raw
  private key: Ethereum hex, Solana Base58, Bitcoin WIF), imports and keeps it
  (with `address` too, the key must derive it); with `address` alone, watches it
  without a key; with neither, generates a key and keeps it. No indexes yet; an
  address already kept answers its wallet (`200`), a watched one gaining its key.
  A wallet's ID is `<chain>:<network>:<address>`.
- `POST /v1/wallets/{id}/indexes` `{asset, start?}`: indexes the wallet in the
  asset from `start` (a native position within `max_history_days`; absent: the
  tip); `PATCH /v1/wallets/{id}/indexes/{asset}` `{index_state}` pauses or
  resumes it; `DELETE` forgets it.
- `GET /v1/wallets`, `GET /v1/wallets/{id}`: every wallet (keyed or watch-only),
  its tags and indexed asset ids.
- `GET /v1/networks/{chain}/{network}/tip/search?at=`: `text/event-stream` of the
  block-by-time search: `probe` events as the range narrows, then `found` or
  `error`.
- `GET /v1/wallets/{id}/balance?asset=`: the signed sum of the address's legs
  (received and minted less spent, burned and fees) and rewards in the asset over
  every live block, in display units, with the position the address is indexed
  through and the indexer's last seen tip.
- `GET /v1/wallets/{id}/transactions?asset=&limit=&cursor=`: transactions in live
  blocks with a leg of the address in the asset, in chain order, with only those
  legs; keyset-paged on (block position, order, txid).
- `POST /v1/wallets/{id}/transactions` `{asset?, transfers}`: enqueues one send
  job (one lane per sender) and answers `202 {job}`; a watch-only wallet is `422`.
- `GET /v1/jobs/{id}`: the job's state, reason, and signed/accepted counts.
- `GET /v1/networks`: every network's indexer stats and stored nodes' origins.
- `GET /health`: every network's indexer stats; ready (`200`) when at least one
  network is configured and none's last step failed, else `503`.

## Layout

Layers, bottom up: `models`, `usecase`, `api`, `main`; `db` and `schema` beside
them. Nothing imports upward (docs/design/069-server-layers.md;
`scripts/server-layers.py` checks it).

- `src/main.rs`: composition (stored nodes into the document, `base::Storage` from
  `usecase::sdk::Stores`, the `payments` builder, the executor with the
  index, expire, triggers and broadcast jobs, HTTP) and shutdown (the executor
  stops after HTTP drains; unfinished jobs resume on the next start).
  `src/main.rs` is the only file that names a chain crate; `src/config.rs` reads
  the file; `src/ui.rs` serves the page.
- `src/db.rs`: the SQLite pool, its pragmas, read and write transactions;
  `src/schema.rs`: the embedded `schema/*.sql` files.
- `src/api/`: one file per resource; `mod.rs` assembles the router and OpenAPI root
  and holds the shared `Error`; `state.rs` is the handler state.
- `src/models/`: one file per table (diesel row and its SQL) and the shared wire
  shapes (`asset`, `network`, `indexing`).
- `src/usecase/`: `sdk/` (every `base` store on the models), `tasks.rs` (the
  job queue), `trigger.rs` (a trigger's decision), the multi-model reads
  (`history.rs`, `wallets.rs`, `chain.rs`, `stats.rs`), and `workers/`: the
  executor and its jobs (`index.rs`, `expire.rs`, `prune.rs`, `broadcast.rs`,
  `trigger/` with `actions.rs`); docs/design/068-index-job.md.

## Notes

- Balances and history include every live block up to the produced tip: the
  store records no confirmation, and `payments` exposes no chain's confirmed tip
  (`base::Stats` carries only the produced `tip`), so no confirmed/pending split
  is reported. A reorg archives blocks and both reads follow.
- The send job signs through `wallet.transaction(asset)` → `transfer` → `sign()`,
  saves every transaction's signed bytes in its payload before broadcasting, and
  broadcasts them in order through `payments.broadcast`; a retry resends the saved
  bytes, never re-signs. Nothing is written to the index: indexing records the
  transactions once mined.
- Generated keys persist directly in SQLite; no encryption or passphrase. Keys
  never appear in API responses, logs or Debug.
- The server speaks plain HTTP. A `bind` that is not loopback is refused unless
  `tls_terminated_upstream = true`.
