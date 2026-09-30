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

Configure at least one chain. Each chain section is that chain crate's own
`Config`; its keys are in the chain's README.

```bash
curl -H "Authorization: Bearer $PAYMENT_API_TOKEN" http://127.0.0.1:8080/v1/assets
curl -H "Authorization: Bearer $PAYMENT_API_TOKEN" http://127.0.0.1:8080/v1/status
```

`payment-server --openapi` prints the OpenAPI document with every route and
body. `GET /health/live`, `GET /health/ready` and `GET /openapi.json` need no
token.

## Notes

- The file holds no secret. The API token, wallet keys and RPC credentials are
  names of environment variables, so the file can be committed and logged.
- The server speaks plain HTTP. A `bind` that is not loopback is refused unless
  `tls_terminated_upstream = true`, which states that a proxy in front of it
  terminates TLS.
- One chain's failure quarantines that chain only; the others keep indexing and
  `GET /v1/status` names the failure. After fixing the cause, resume it with
  `POST /v1/status/resume` and the body
  `{"network": {"chain": "bitcoin", "name": "mainnet"}}`.
- A `confirmed_reorg` failure cannot be resumed. Stop the server and run
  `payment-server ./payments.toml --rescan bitcoin/mainnet`. It deletes that
  network's indexed rows and keeps its watched addresses, and the next start
  indexes it again. Running it against a live server would serve an empty
  history that looks correct.
- `GET /health/ready` answers `204` only when every network is ready or
  quarantined and at least one is ready.
