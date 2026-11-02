# Payment SDK

A Rust SDK and one server for wallets on Bitcoin, Ethereum and Solana: generate
or import keys, watch addresses, read exact balances and history, and send
transfers. Everything runs in one process over one SQLite file, and it recovers
from restarts and reorgs.

## Run the server

```bash
cat > payments.toml <<'TOML'
bind = '127.0.0.1:3000'
bearer_token_env = 'PAYMENT_API_TOKEN'

[chains.ethereum]
network = 'anvil'
chain_id = 31337
rpc.endpoints = ['http://127.0.0.1:8545']
TOML

anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0 &

export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- ./payments.toml

curl -H "Authorization: Bearer $PAYMENT_API_TOKEN" http://127.0.0.1:3000/v1/assets
```

The full configuration is in [apps/server](apps/server/README.md).

## Folders

| Folder | What it holds |
| --- | --- |
| [apps/server](apps/server/README.md) | The executable: configuration, storage, supervision and the HTTP API |
| [sdk/base](sdk/base/README.md) | Chain-neutral values and the signing and broadcast traits |
| [sdk/chains/bitcoin](sdk/chains/bitcoin/README.md) | Bitcoin: RPC, blocks, UTXO wallets, sending |
| [sdk/chains/ethereum](sdk/chains/ethereum/README.md) | Ethereum: RPC, blocks, ETH and ERC-20 wallets, sending |
| [sdk/chains/solana](sdk/chains/solana/README.md) | Solana: RPC, blocks, SOL and SPL wallets, SOL sending |
| [sdk/indexing](sdk/indexing/README.md) | Watches, block synchronization and the store contracts |
| [sdk/payments](sdk/payments/README.md) | The configuration document and chain registration |
| [packages/crypto](packages/crypto/README.md) | Keys and signing |
| [packages/decimal](packages/decimal/README.md) | Exact decimal amounts |
| [packages/rpc](packages/rpc/README.md) | JSON-RPC client with retry and failover |
| [packages/http-server](packages/http-server/README.md) | Authenticated axum serving |
| [docs/design](docs/design) | Design notes |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Ownership and dependency direction |

## Notes

- Never point a test or an example at a funded key or a public broadcast
  endpoint. Tests use local RPC doubles and temporary databases.
