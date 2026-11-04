# Payment SDK

A Rust SDK and one server for wallets on Bitcoin, Ethereum and Solana: generate
or import keys, watch addresses, read exact balances and history, and send
transfers. Everything runs in one process over one SQLite file, and it recovers
from restarts and reorgs.

## Run the server

```bash
# Once: the database with the seeded test and main networks (apps/server/seed.sql).
cargo run --locked -p payment-server -- migrate

export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- serve --bind 127.0.0.1:3000 \
  --bearer-token-env PAYMENT_API_TOKEN

curl -H "Authorization: Bearer $PAYMENT_API_TOKEN" http://127.0.0.1:3000/v1/assets
```

Flags and the network tables are in [apps/server](apps/server/README.md).

## Folders

| Folder | What it holds |
| --- | --- |
| [apps/server](apps/server/README.md) | The executable: storage, supervision and the HTTP API |
| [sdk/base](sdk/base/README.md) | Chain-neutral values and the signing and broadcast traits |
| [sdk/chains/bitcoin](sdk/chains/bitcoin/README.md) | Bitcoin: RPC, blocks, UTXO wallets, sending |
| [sdk/chains/ethereum](sdk/chains/ethereum/README.md) | Ethereum: RPC, blocks, ETH and ERC-20 wallets, sending |
| [sdk/chains/solana](sdk/chains/solana/README.md) | Solana: RPC, blocks, SOL and SPL wallets, SOL sending |
| [sdk/indexing](sdk/indexing/README.md) | Watches, block synchronization and the store contracts |
| [sdk/payments](sdk/payments/README.md) | The network container and chain registration |
| [packages/crypto](packages/crypto/README.md) | Keys and signing |
| [packages/decimal](packages/decimal/README.md) | Exact decimal amounts |
| [packages/rpc](packages/rpc/README.md) | JSON-RPC client with retry and failover |
| [packages/http-server](packages/http-server/README.md) | Authenticated axum serving |
| [docs/design](docs/design) | Design notes |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Ownership and dependency direction |

## Notes

- Never point a test or an example at a funded key or a public broadcast
  endpoint. Tests use local RPC doubles and temporary databases.
