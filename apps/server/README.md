# payment-server

The one executable of the workspace: it reads a configuration file, connects the
configured Bitcoin, Ethereum and Solana nodes, indexes into one SQLite file and
serves the wallet HTTP API. It is a binary with no library.

## Usage

```bash
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- migrate ./payments.toml  # optional: create the database
cargo run --locked -p payment-server -- serve ./payments.toml
```

`migrate` opens the configured database file: an empty file gets the schema,
a file already holding this schema is left as it is (re-running is a no-op),
any other file is refused. `serve` does the same on start, then serves.

```toml
# payments.toml (a `.toml` path is read as TOML, anything else as JSON)
bind = '127.0.0.1:8080'
bearer_token_env = 'PAYMENT_API_TOKEN'   # the variable that holds the API token
storage = { path = 'payment.db' }        # optional, this is the default

[chains.bitcoin]
network = 'regtest'
confirmation_depth = 6
rpc.endpoints = ['http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443']

[chains.ethereum]
network = 'mainnet'
chain_id = 1
rpc.endpoints = ['https://node.invalid/v3/${ETH_RPC_KEY}']

[chains.solana]
network = 'mainnet'
genesis_hash = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d'
rpc.endpoints = ['https://node.invalid']
```

Wallets are created through the API and restored from the database on restart.
Public addresses can be observed through the API.

Chains are composed once, at startup, from this file; edit it and restart to add
or remove chains. A chain whose node fails verification at startup is skipped and
named on stderr; the others serve. Chains are optional. Each section is the chain
crate's own `Config`; its keys are in the chain's README. `bearer_token_env` is
optional only for a loopback `bind`.

A section's `rpc.endpoints` are only the bootstrap: the first start stores them in
the database, and from then on the stored list is what the network uses, whatever
the file says. Change nodes while running with
`PUT /v1/networks/{chain}/{network}/rpc` (or the page's Networks tab): the chain's
own startup check runs over the new nodes, and only a list that passes is stored and
used for later calls. `POST /v1/networks/{chain}/{network}/rescans` purges one
network's indexed facts and indexes it again from every watch's birthday, while the
other networks keep running. `GET /v1/networks` lists every configured network, its
state and its nodes' origins.

The server also serves a browser page at `/`: wallets, networks, and the API reference
rendered from `GET /openapi.json` with a request console. `[ui] brand` names it,
and `PAYMENT_UI_API_BASE` points it at another origin when a proxy fronts the API.

```bash
curl -H "Authorization: Bearer $PAYMENT_API_TOKEN" http://127.0.0.1:8080/v1/assets
curl http://127.0.0.1:8080/health
```

`payment-server --openapi` prints the OpenAPI document with every route and
body; `docs/openapi.json` is the committed copy. `GET /health`, `GET /openapi.json`
and the page need no token. `POST /v1/wallets` generates a wallet, or watches an
`address` without its key, with an optional `label`; the server mints a UUIDv7 ID.
`start` is `"fresh"` (the default: the next block) or a native position; a start
the network's backfill policy refuses answers `400`. HTTP accepts no private keys.
`DELETE /v1/wallets/{id}` archives a wallet: its address is no longer observed,
its signer is dropped, its indexed history and balance stay readable, and sends
from it answer `409`. Archiving is final: creating its address again answers `409`.
`GET /v1/wallets` leaves archived wallets out unless `archived=true`. Single and
batch transfers accept an exact decimal amount or `"all"` to send the available
balance after chain fees.

## Layout

- `src/main.rs`: composition and startup (stored nodes, `compose_available`, the
  send queues, wallet restore, supervision, shutdown order) and the candidate-node
  verifier the network routes call. `src/chains.rs` is the only file that names a
  chain crate; `src/config.rs` reads the file; `src/storage.rs` opens the one
  SQLite file; `src/operator.rs` runs the offline commands; `src/ui.rs` serves the
  page.
- `src/api/`: one file per endpoint group (`assets.rs`, `wallets.rs`,
  `balance.rs`, `transactions.rs`, `networks.rs`, `health.rs`, `openapi.rs`);
  `mod.rs` assembles the router and OpenAPI root and holds the shared `Error`.
  `src/state.rs` is the handler state and `src/asset.rs` the asset tag. None of
  them names a chain crate, `sdk_config` or a storage backend
  (`scripts/check-chains.sh` check 8).
- `src/models/`: the nouns the endpoints and the tables share (`asset`, `block`,
  `completeness`, `network`, `wallet`, `endpoint`), each with its own serde/utoipa
  wire form and, for `wallet` and `endpoint`, its SQL. No axum, no chain crate.

## Notes

- Generated keys persist directly in SQLite; no encryption or passphrase.
  Keys never appear in API responses, logs or Debug.
- One SQLite file, one `sqlx` pool (`indexing::sqlite::Database::open_with`).
  Wallets are indexing's `wallet` rows, read and filed through
  `indexing::WalletRegistry`; the server's own table, `rpc_endpoints`, is
  [`src/models/schema.sql`](src/models/schema.sql), created with indexing's when the
  file is new. No migrations: the file's identity fingerprints both schemas, so a
  change refuses the old file ("delete the database file"). Index rescans keep every
  wallet, label and key.
- `POST /v1/wallets`: admit, generate a key or parse the address, file the row
  archived and unobserved (`register`, so the key is durable first), then
  `index.watch` it from the requested `start`, then keep the signer. An interrupted
  or refused creation leaves an archived, never-observed row, which the next
  creation of that address takes over. An address of a wallet that was observed and
  archived is refused with 409; nothing is restored.
- `DELETE /v1/wallets/{id}`: `index.unwatch` archives the row under the publication
  permit; a wallet of a network this process does not serve is archived by the
  registry directly. History and balance stay readable.
- At startup every active, keyed wallet of a served network and configured asset is
  rebuilt as a signer; nothing is watched or unwatched, because the index restores
  its watch list from the same rows. A wallet whose asset is no longer configured
  is listed with `asset: null` and its `asset_id`.
- The API token and RPC credentials use environment references (`${NAME}` in an
  endpoint URL or header); never log the configuration.
- The server speaks plain HTTP. A `bind` that is not loopback is refused unless
  `tls_terminated_upstream = true`, which states that a proxy in front of it
  terminates TLS.
- One chain's failure quarantines that chain only; the others keep indexing.
  `GET /health` reports each network, sanitized RPC counters and each asset's
  in-memory send queue from memory.
  It answers `200` only when every network is ready or quarantined and at least
  one is ready; otherwise it answers `503` with the same report shape.
- `rpc_endpoints` holds one JSON list per network, 1 to 16 nodes in failover order.
  Rows may hold literal credentials; prefer `${NAME}` references. The API
  returns only each node's origin. A network skipped at startup is listed
  `unavailable`; replacing its nodes takes effect at the next start.
- Storage opens before any node is contacted: the stored nodes replace each
  section's, and a network with no row stores the file's.
- During a rescan, wallet creation, archiving and sends on that network answer `409`
  (`wallets::Wallets::pause`, which also waits for those already running); reads
  answer from the purged store, marked incomplete. The rescan itself is
  `indexing::SyncControl::rescan`. A second rescan answers `409` and a node change
  `503` until it finishes.
- After fixing a quarantine's cause, rescan the network or restart the server.
- A confirmed reorg requires a rescan: `POST .../rescans`, or with the server stopped
  `payment-server serve ./payments.toml --rescan bitcoin/mainnet`. Both delete only that
  network's indexing facts and preserve watches, birthdays and wallets.
- To abandon unfinished historical catch-up, stop the server and run
  `payment-server serve ./payments.toml --cancel-backfill bitcoin/mainnet`. Existing
  indexed facts remain; unfinished watches begin after the saved checkpoint.
  Both offline commands validate the exact configured network and never start
  RPC, supervision, or a listener. Never run them alongside the live server.
