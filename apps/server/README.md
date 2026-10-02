# payment-server

The one executable of the workspace: it reads a configuration file, connects the
configured Bitcoin, Ethereum and Solana nodes, indexes into one SQLite file and
serves the wallet HTTP API. It is a binary with no library.

## Usage

```bash
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- ./payments.toml
```

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

# Optional: keys imported at every start. `start` is a block position or 'fresh'.
[[wallets]]
id = 'treasury'
asset = 'btc'
secret_env = 'BTC_TREASURY_KEY'
start = 'fresh'
```

Chains are optional: the UI starts with no nodes or unavailable nodes. File sections
seed the `chains` table once; later changes use the Chains UI. Each section is the
chain crate's own `Config`; its keys are in the chain's README.

The Chains sidebar groups networks under each chain, with connection indicators.
Each network has a bookmarkable view: RPC domain cards, indexing status, then
combined indexing/RPC logs. Cards show URL origins; clicking a card prefills its
configured RPC URL for editing. Environment references stay unresolved and
authentication headers are not returned. URL edits preserve existing headers and bounds.
The sidebar loads chain/network choices from `ui/src/chains.json` before API
responses, then adds custom networks from the backend.
Each network view has **+ Add RPC node**. Chain, network and native asset choices
are predefined; users enter node details and RPC URLs, without JSON editing.
Known network identity parameters are filled from the catalog; existing settings
are preserved. Solana preset genesis hashes match the [upstream constants](https://github.com/solana-labs/solana/blob/master/sdk/src/genesis_config.rs).
Custom Solana clusters need their own configured genesis hash and remain visible
when configured; the catalog does not invent one for local validators.

```bash
curl -H "Authorization: Bearer $PAYMENT_API_TOKEN" http://127.0.0.1:8080/v1/assets
curl http://127.0.0.1:8080/healthz
```

`payment-server --openapi` prints the OpenAPI document with every route and
body. `GET /healthz` and `GET /openapi.json` need no token. Wallet creation
and address watching begin at the current tip; HTTP does not accept birthdays
or private keys. Single and batch transfers accept an exact decimal amount or
`"all"` to send the available balance after chain fees.

## Notes

- Generated and imported keys persist directly in SQLite; no encryption or passphrase.
  Keys never appear in API responses, logs or Debug.
- The application owns `wallet` and `wallet_schema`; index rescans preserve them.
  Key insertion precedes watch publication. Interrupted creation leaves a private,
  unpublished key; restart restores only keys with matching durable watches.
- The plaintext registry uses schema 2. Older encrypted and unknown schemas are
  refused without changing their rows; this build does not reinterpret ciphertext.

- API tokens and imported wallet keys use environment references. RPC settings may contain
  literal credentials and are stored directly in `chains`; never log the configuration.
- The server speaks plain HTTP. A `bind` that is not loopback is refused unless
  `tls_terminated_upstream = true`, which states that a proxy in front of it
  terminates TLS.
- One chain's failure quarantines that chain only; the others keep indexing.
  `GET /healthz` reports each network and sanitized RPC counters from memory.
  It answers `200` only when every network is ready or quarantined and at least
  one is ready; otherwise it answers `503` with the same report shape.
- After fixing a quarantine's cause, request Resume in the Chains UI.
- A confirmed reorg requires Reindex in the UI, or stopping the server and running
  `payment-server ./payments.toml --rescan bitcoin/mainnet`. Recovery deletes only
  that network's indexing facts and preserves watches, birthdays and keys.
- To abandon unfinished historical catch-up, stop the server and run
  `payment-server ./payments.toml --cancel-backfill bitcoin/mainnet`. Existing
  indexed facts remain; unfinished watches begin after the saved checkpoint.
  Both offline commands validate the exact configured network and never start
  RPC, supervision, or a listener. Never run them alongside the live server.
- Unpublished keys remain private after interrupted creation.
  No orphan-listing command is provided until a complete watch query can prove
  absence across every network, including networks disabled in configuration.

## Events and controls

The server injects one bounded `logging::Memory` sink into RPC nodes, indexing and
its repository. `GET /v1/logs` filters by `chain`, `network`, `node_id`, `component`
and an `after` cursor; records are notifications, not durable delivery. The Chains
view shows actual status, node counters and filtered logs. `PUT /v1/indexing` accepts
`{"network":{"chain":"bitcoin","name":"mainnet"},"action":"pause"}`, `resume`,
or `reindex`. A 202 response means the request is saved; poll `GET /v1/chains` for
applied state. Pause finishes the active bounded pass. Reindex drains network
operations, purges confirmed facts, resets pending/cache state and scans again.
Other networks continue; unsafe reads and sends stay blocked during rebuilding.

`GET /v1/chains` omits RPC URLs and credentials. `PUT /v1/chains/{chain}` saves
revision-checked settings; omit an existing node's endpoint to retain it. Nodes have
stable IDs and priority order. Candidates are verified before activation; failed
candidates retain the working connection. Switching drains complete operations and
keeps the same wallet factories/senders. Networks can be added; existing network
and asset parameters cannot be edited or deleted through this API.

The database ownership, recovery phases and verification limits are documented in
[runtime controls](../../docs/design/runtime-controls.md).
