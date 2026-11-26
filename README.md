# hashinbox

A Rust SDK and one server for wallets on Bitcoin, Ethereum and Solana: generate
or import keys, watch addresses, read exact balances and history, and send
transfers. Everything runs in one process over one SQLite file, and it recovers
from restarts and reorgs.

## Run the server

```bash
# The first start creates the database with the seeded test and main networks
# (apps/server/seed.sql).
cargo run --locked -p hashinbox -- serve --bind 127.0.0.1:3000

# Once: the root user (no user yet), answered with its session.
curl -X POST http://127.0.0.1:3000/v1/users -H 'content-type: application/json' \
  -d '{"email": "me@example.com", "name": "me", "password": "replace-me"}'
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:3000/v1/assets
```

Flags and the network tables are in [apps/server](apps/server/README.md).

## Deploy with a preset config

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

## Folders

| Folder | What it holds |
| --- | --- |
| [apps/server](apps/server/README.md) | The executable: storage, supervision and the HTTP API |
| [sdk/base](sdk/base/README.md) | Chain-neutral values and the signing and broadcast traits |
| [sdk/chains/bitcoin](sdk/chains/bitcoin/README.md) | Bitcoin: RPC, blocks, UTXO wallets, sending |
| [sdk/chains/ethereum](sdk/chains/ethereum/README.md) | Ethereum: RPC, blocks, ETH and ERC-20 wallets, sending |
| [sdk/chains/solana](sdk/chains/solana/README.md) | Solana: RPC, blocks, SOL and SPL wallets, SOL sending |
| [sdk/indexing](sdk/indexing/README.md) | Watches, block synchronization and the store contracts |
| [packages/crypto](packages/crypto/README.md) | Keys and signing |
| [packages/decimal](packages/decimal/README.md) | Exact decimal amounts |
| [packages/rpc](packages/rpc/README.md) | JSON-RPC over HTTP, one node per client, with retry |
| [packages/http-server](packages/http-server/README.md) | Authenticated axum serving |
| [docs/design](docs/design) | Design notes |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Ownership and dependency direction |

## Notes

- Never point a test or an example at a funded key or a public broadcast
  endpoint. Tests use local RPC doubles and temporary databases.
