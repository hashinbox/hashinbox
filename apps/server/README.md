# payment-server

The one executable of the workspace: it reads a configuration file, connects the
configured Bitcoin, Ethereum and Solana nodes, indexes into one SQLite file and
serves the wallet HTTP API. It is a binary with no library.

## Usage

```bash
export PAYMENT_API_TOKEN='replace-me'
export PAYMENT_WALLET_PASSPHRASE='a-long-unique-secret-kept-outside-the-database'
cargo run --locked -p payment-server -- ./payments.toml
```

```toml
# payments.toml (a `.toml` path is read as TOML, anything else as JSON)
bind = '127.0.0.1:8080'
wallet_passphrase_env = 'PAYMENT_WALLET_PASSPHRASE' # optional variable name; value required
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
curl http://127.0.0.1:8080/healthz
```

`payment-server --openapi` prints the OpenAPI document with every route and
body. `GET /healthz` and `GET /openapi.json` need no token. Wallet creation
and address watching begin at the current tip; HTTP does not accept birthdays
or private keys. Single and batch transfers accept an exact decimal amount or
`"all"` to send the available balance after chain fees.

## Notes

- Generated and imported keys persist encrypted in the same SQLite file. Keep
  the passphrase: a missing or wrong one prevents startup; there is no plaintext
  fallback or key-return endpoint. Back up the database and passphrase separately.
- The application owns `wallet` and `wallet_schema`; index rescans leave them
  untouched. Key insertion precedes watch publication. A key whose watch failed
  stays private; it is not automatically registered during restart.
- Encryption uses ring AES-256-GCM, random nonces, an authenticated ID/asset tag,
  and a per-file 32-byte salt with PBKDF2-HMAC-SHA256 (600,000 iterations).
  The passphrase and decrypted key buffers are zeroized. This is in-process
  custody, not an HSM or remote custody service.

- The configuration file holds no secret. The API token, wallet keys and RPC credentials are
  names of environment variables, so the file can be committed and logged.
- The server speaks plain HTTP. A `bind` that is not loopback is refused unless
  `tls_terminated_upstream = true`, which states that a proxy in front of it
  terminates TLS.
- One chain's failure quarantines that chain only; the others keep indexing.
  `GET /healthz` reports each network and sanitized RPC counters from memory.
  It answers `200` only when every network is ready or quarantined and at least
  one is ready; otherwise it answers `503` with the same report shape.
- After fixing a quarantine's cause, restart the process. Quarantine is process
  state; there is no HTTP resume route or offline resume command.
- A `confirmed_reorg` requires stopping the server and running
  `payment-server ./payments.toml --rescan bitcoin/mainnet`. This deletes only
  that network's indexing facts and preserves watches and application keys.
- To abandon unfinished historical catch-up, stop the server and run
  `payment-server ./payments.toml --cancel-backfill bitcoin/mainnet`. Existing
  indexed facts remain; unfinished watches begin after the saved checkpoint.
  Both offline commands validate the exact configured network and never start
  RPC, supervision, or a listener. Never run them alongside the live server.
- Unpublished keys remain encrypted and hidden after interrupted creation.
  No orphan-listing command is provided until a complete watch query can prove
  absence across every network, including networks disabled in configuration.
