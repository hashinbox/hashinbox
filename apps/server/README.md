# hashinbox

The one executable of the workspace: it opens one SQLite file, builds the
container (`base::Container`) of its network tables with the Bitcoin,
Ethereum and Solana chains registered, each network over its stored nodes
(`usecase::Transport`), runs every indexed network's indexer and the send
queue, and serves the wallet HTTP API. It is a binary with no library.

## Usage

```bash
cargo run --locked -p hashinbox -- serve   # the first start creates the database
# Once: POST /v1/users creates the root user (design 096).
```

Flags, each with an environment fallback: `--db` (`HASHINBOX_DB`, default
`.data/hashinbox.db`), `--bind` (`HASHINBOX_BIND`, default `127.0.0.1:8080`),
`--tls-terminated-upstream`
(`HASHINBOX_TLS_TERMINATED_UPSTREAM`), `--config` (`HASHINBOX_CONFIG`, below).

`serve` creates a new file (absent or empty) with `init.sql` then `seed.sql`,
and opens an existing one as it is; there are no migrations. Networks are rows: `network` (facts and
`settings`, the chain's own knobs as JSON), `network_asset`, `network_endpoint`
(one row per node, see `seed.sql`) and `network_endpoint_route` (methods sent
round robin over some of a network's nodes). The server indexes exactly the
`enabled` networks; a network's `network_index` row is written by its first
step that knows the tip (no row: no tip yet); each is built by its chain (`ServiceFactory::new(&config)`), asking
no node, and one that does not build fails only its own jobs, with why.

A network's transport (`usecase::Transport`) is its stored nodes and the
selection over them, made on every call: a routed method rotates over its
route's nodes; anything else goes to the healthy node with the highest tip
(each tip asked at most once per block time), then the others in stored
order, a node more than `confirmations` behind after those in sync. Three
retryable failures in a row, or one rate limit, put a node last until it
answers or 10 block times (at least 30 s) pass. A retryable failure moves on
to the next node; a broadcast reaches every node. Adding a node asks it which
chain it serves first (`Transport::verify`); every endpoint or route edit is
stored, then loaded into the running transport (`usecase::Networks::load`):
a node whose URL stays keeps its tip, failures and call counts. `GET
/v1/networks` shows each node's `state`. A node's URL carries its own
`rpc.` settings (`packages/rpc/README.md`): `rpc.max_batch=4&rpc.rps=3.5`
sends a batch as requests of 4 calls, paced to 3.5 calls a second; a
request waits for its turn rather than fail.

### Deploy with a preset config

`serve --config` (`HASHINBOX_CONFIG`) seeds an empty database: the root user
and settings, in one write, checked as `POST /v1/users`, `PUT
/v1/settings/{key}` and `POST /v1/recovery` check them. The value is inline
JSON (it starts with `{`) or a JSON file's path:

```json
{
  "root": { "email": "ops@shop.io", "name": "Ops", "password": "at least 8 chars" },
  "settings": { "instance": "Shop", "recovery_passphrase": "twelve bip39 words …" }
}
```

```bash
docker run -e HASHINBOX_CONFIG='{"root":{"email":"ops@shop.io","name":"Ops","password":"…"}}' …
# or a mounted file
docker run -v ./bootstrap.json:/etc/hashinbox/bootstrap.json:ro \
  -e HASHINBOX_CONFIG=/etc/hashinbox/bootstrap.json …
```

- No user yet: the root is created and the settings applied, or the server
  does not start (a bad field, a short password, a bad mnemonic, a config
  with no `root`), naming the field and writing nothing.
- A user exists: the config is not read; users and settings are never
  overwritten.
- Keep the file at mode 0600, change the password after the first login; the
  config only seeds an empty database. Its content is never logged.

The server also serves a browser page at `/` (`HASHINBOX_UI_API_BASE`).
The committed page (`ui/dist`) was built against the previous API and needs a
rebuild for the routes below.

## API

`hashinbox --openapi` prints the OpenAPI document; `docs/openapi.json` is the
committed copy. `GET /health`, `GET /v1/version` and `GET /openapi.json` need no
token; every other route takes a session JWT or a key's secret (design 096).

- `GET /v1/assets`: every configured asset (`container.assets()`): its network, its
  id (`native` or a token's) and metadata.
- `POST /v1/wallets` `{chain, network, secret?, address?}`: with `secret` (a raw
  private key: Ethereum hex, Solana Base58, Bitcoin WIF), imports and keeps it
  (with `address` too, the key must derive it); with `address` alone, watches it
  without a key; with neither, generates a key and keeps it. No indexes yet; an
  address already kept answers its wallet (`200`), a watched one gaining its key.
  A wallet's ID is `<chain>:<network>:<address>`.
- `POST /v1/wallets/{id}/indexes` `{asset, start?}`: indexes the wallet in the
  asset from `start` (a native position at or above the tip less
  `max_depth`; absent: the tip; a chain that reads balances starts at its
  final block, balances null until its balance read there appends an
  `adjustment` leg); `PATCH /v1/wallets/{id}/indexes/{asset}`
  `{index_state}` pauses or resumes it; `DELETE` forgets it.
  `POST .../{asset}/reconcile` (Solana, Ethereum) deletes its legs above the
  final block and unsets it: balances null until the chain's balance read
  appends an `adjustment` leg and it reads on.
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
- `POST /v1/wallets/{id}/transactions` `{asset?, destination, amount}`: enqueues
  one send job, one payment as one transaction (one lane per sender), and
  answers `202 {job}`; a watch-only wallet is `422`.
- `GET /v1/jobs/{id}`: the job's state, reason, and for a send its `txid` and
  whether a node `accepted` it.
- `GET /v1/networks`: every `network` row with its `assets`, `endpoints`,
  `index` (its `network_index` row, null before its first tip) and the node's
  tip asked now.
- `GET /health`: every indexed network with its `network_index` row (null
  before its first tip); ready (`200`) when at least one network is indexed
  and none's last step failed, else `503`.

## Layout

Layers, bottom up: `models`, `usecase`, `api`, `main`; `db` beside
them. Nothing imports upward (docs/design/069-server-layers.md;
`scripts/server-layers.py` checks it).

- `src/main.rs`: one `main` block: the container (`base::Storage` from
  `usecase::sdk::Stores`, every stored network registered per chain by
  `usecase::Networks`), `AppState`, the workers (`usecase::workers::executor`:
  index, prune, triggers and broadcast jobs), HTTP; and shutdown (the
  executor stops after HTTP drains; unfinished jobs resume on the next start).
  `src/main.rs` is the only file that names a chain crate; `src/config.rs` is
  the command line; `src/ui.rs` serves the page.
- `src/db.rs`: the SQLite pool, its pragmas, read and write transactions, and
  `open_or_create`, which runs the embedded `init.sql` and `seed.sql` on a new file.
- `src/api/`: one file per resource; `mod.rs` assembles the router and OpenAPI root
  and `src/error.rs` holds the shared `Error`; `state.rs` is the handler state.
- `src/models/`: one file per table (diesel row and its SQL) and the shared wire
  shapes (`asset`, `network`).
- `src/usecase/`: `sdk/` (every `base` store on the models), `transport.rs`
  (a network's nodes and the selection over them), `networks.rs` (every
  stored network into the container, and its edits into its transport),
  `tasks.rs` (the job queue: `tasks::Store` on SQLite, wrapped in
  `tasks::Notifying`),
  `trigger.rs` (a trigger's decision), the multi-model reads (`history.rs`,
  `wallets.rs`, `chain.rs`, `stats.rs`), and `workers/`: the jobs, each
  declaring its `tasks::Policy` (`index.rs`, `prune.rs`, `broadcast.rs`,
  `trigger/` with `actions.rs`), run by `tasks::Executor`;
  docs/design/068-index-job.md, 076-jobs.md.

## Notes

- Balances and history include every live block up to the produced tip: the
  store records no confirmation, and the container exposes no chain's confirmed tip
  (`base::Stats` carries only the produced `tip`), so no confirmed/pending split
  is reported. A reorg deletes the blocks it takes back and both reads follow.
- The send job signs through `wallet.transaction(asset)` → `transfer` → `sign()`,
  saves every transaction's signed bytes in its payload before broadcasting, and
  broadcasts them in order through `container.broadcast`; a retry resends the saved
  bytes, never re-signs. With each transaction it records its transaction index
  (design 085), which the indexer moves with every block: the job reads its
  payment's status there alone, and indexing records the transaction once mined.
- Generated keys persist directly in SQLite; no encryption or passphrase. Keys
  never appear in API responses, logs or Debug.
- The server speaks plain HTTP. A `bind` that is not loopback is refused unless
  `--tls-terminated-upstream`.
