# payment-server

The one executable of the workspace: it opens one SQLite file, builds
`payments::Payments` from its network tables with the Bitcoin, Ethereum and
Solana chains registered, runs every indexed network's indexer and the send
queue, and serves the wallet HTTP API. It is a binary with no library.

## Usage

```bash
cargo run --locked -p payment-server -- migrate   # once: create the database
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- serve --bearer-token-env PAYMENT_API_TOKEN
```

Flags, each with an environment fallback: `--db` (`PAYMENT_DB`, default
`.data/payment.db`), `--bind` (`PAYMENT_BIND`, default `127.0.0.1:8080`),
`--bearer-token-env` (`PAYMENT_BEARER_TOKEN_ENV`: the variable holding the API
token; optional only for a loopback bind), `--tls-terminated-upstream`
(`PAYMENT_TLS_TERMINATED_UPSTREAM`), `--brand` (`PAYMENT_UI_BRAND`).

`migrate` executes `init.sql` then `seed.sql` on the (new) file; there are no
migrations. `serve` only opens it. Networks are rows: `network` (facts and
`settings`, the chain's own knobs as JSON), `network_asset`, `network_endpoint`
(one row per node, see `seed.sql`) and `network_endpoint_route` (methods sent
round robin over some of a network's nodes, `rpc::Router`). The server indexes exactly the networks with a
`network_index` row and adds one for every `enabled` network at start; each is
built by its chain (`TryFrom<&base::NetworkConfig>`), and one that fails
verification stops the start.

`PUT /v1/networks/{chain}/{network}/rpc` stores a list and moves every later call
of the network to it at once (`base::Cluster` behind the network's
`rpc::Swap`), unverified; the chain verifies it on the next start.

The server also serves a browser page at `/` (`--brand`, `PAYMENT_UI_API_BASE`).
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
- `GET /v1/networks`: every `network` row with its `assets`, `endpoints`,
  `index` (its `network_index` row) and the node's tip asked now.
- `GET /health`: every `network_index` row; ready (`200`) when at least one
  network is indexed and none's last step failed, else `503`.

## Layout

Layers, bottom up: `models`, `usecase`, `api`, `main`; `db` beside
them. Nothing imports upward (docs/design/069-server-layers.md;
`scripts/server-layers.py` checks it).

- `src/main.rs`: composition (network rows into the `payments` builder, `base::Storage` from
  `usecase::sdk::Stores`, the `payments` builder, the executor with the
  index, expire, triggers and broadcast jobs, HTTP) and shutdown (the executor
  stops after HTTP drains; unfinished jobs resume on the next start).
  `src/main.rs` is the only file that names a chain crate; `src/config.rs` is
  the command line; `src/ui.rs` serves the page.
- `src/db.rs`: the SQLite pool, its pragmas, read and write transactions, and
  `migrate`, which runs the embedded `init.sql` and `seed.sql`.
- `src/api/`: one file per resource; `mod.rs` assembles the router and OpenAPI root
  and holds the shared `Error`; `state.rs` is the handler state.
- `src/models/`: one file per table (diesel row and its SQL) and the shared wire
  shapes (`asset`, `network`).
- `src/usecase/`: `sdk/` (every `base` store on the models), `tasks.rs` (the
  job queue: `tasks::Store` on SQLite, wrapped in `tasks::Notifying`),
  `trigger.rs` (a trigger's decision), the multi-model reads (`history.rs`,
  `wallets.rs`, `chain.rs`, `stats.rs`), and `workers/`: the jobs, each
  declaring its `tasks::Policy` (`index.rs`, `expire.rs`, `prune.rs`,
  `broadcast.rs`, `trigger/` with `actions.rs`), run by `tasks::Executor`;
  docs/design/068-index-job.md, 076-jobs.md.

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
  `--tls-terminated-upstream`.
