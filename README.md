# Payment SDK

Payment-SDK is a design-stage Rust workspace whose current implementation is one
Bitcoin/Ethereum/Solana wallet API process. It composes native chain clients,
wallet implementations, embedded indexers, and one storage backend — a single
SQLite file — in one process. There are no internal HTTP hops or separately
deployed wallet/indexer services.

Native SOL submission and runtime composition are implemented through the same
chain-neutral wallet and indexing contracts. The checksum-pinned owned Agave
system target is explicit but remains manual evidence; see
`docs/SYSTEM_REQUIREMENTS.md` for the tested and unavailable boundary.

The current scope is deliberately small:

- generate or import wallets through one chain-neutral `Wallets` collection;
- index the authoritative wallet address/birthday set with one reusable
  `Indexing` contract;
- read exact selected-asset balances and complete checkpoint-bound history for
  each wallet's configured payment asset;
- submit one transfer or a non-empty ordered batch; and
- survive restarts and retained reorgs without serving orphan history.

Deposit accounting, ledgers, collections/sweeps, payment workflows, custody
services, hardware wallets, raw-block archives, and indexing command APIs are
not part of the workspace.

## Current workspace

The [crate map in `ARCHITECTURE.md`](ARCHITECTURE.md#crate-map) lists every
crate with its purpose, layer, and README; each README is also that crate's
rustdoc (`cargo doc --workspace --no-deps --open`).

`apps/server` is deliberately the visible object graph, in the same shape as
`examples/all_chains/main.rs`. `src/chains.rs` is the only code in the
workspace that names a chain crate, and all it does is register each chain's own
`Config` under the section name that chain parses: the chain then verifies one
long-lived RPC client and composes its index service and its wallet factories,
under their asset tags, over the one repository opened on the configured index
file. `compose` puts the indexes
into one multi-chain `Index` and the factories into one `Wallets<AssetTag>`.
Everything after that, `main` included, uses only those two abstractions: the
index owns the list of what it observes, so the sync task hands the supervisor
no selection at all, and the routes (`apps/api`, a library with no chain
dependency) receive the factory map, the wallets this process holds keys for,
the indexer, and readiness state.

Index persistence asks a store for two things and nothing more, together
`indexing::Repository`: `WatchStore` is the durable mirror of what the index
watches, handed back with the block through which each observation has been
inspected, and
`IndexStore` holds the checkpoint and takes the one write a pass makes. A
store may also offer what this workspace's own consumers read —
`BalanceStore` for the fold over its history rows, `OutputStore` for current
live UTXOs, `Purging` for the offline rescan — each its own trait, so a sink
that answers no query at all is a legal store rather than a stubbed one.
Querying history is not among them: it is the repository's own
`ConfirmedHistory` API, and `indexing::ObservedHistory` is what joins one
repository's confirmed pages to one index's overlay so a consumer can read
confirmed *plus* pending. The repository stores only checkpoint, canonical
history, live outputs, and those watch rows — never key material.

Dependencies point toward generic contracts. Packages import no SDK/application
crate; indexing imports no concrete chain or stored row; chain-native
transaction and RPC semantics stay in each chain crate. After composition,
business and endpoint code do not branch on a concrete chain.

## Run it

The server is one binary that takes one JSON configuration file. A minimal
single-node run needs a bearer token in the environment, one chain section, and
nothing else: storage defaults to one SQLite file (`payment.db`, resolved
against the working directory), so no database service is required.

```bash
cat > config.json <<'JSON'
{
  "bind": "127.0.0.1:3000",
  "bearer_token_env": "PAYMENT_API_TOKEN",
  "chains": {
    "ethereum": {
      "network": "anvil",
      "chain_id": 31337,
      "rpc": { "endpoints": ["http://127.0.0.1:8545"] }
    }
  }
}
JSON

anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0 &

export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- ./config.json

curl -s localhost:3000/health/ready -o /dev/null -w '%{http_code}\n'
curl -s -H "Authorization: Bearer $PAYMENT_API_TOKEN" localhost:3000/v1/assets
```

Every setting, route, and status code is in [`docs/API.md`](docs/API.md).

The library is easier to meet through the examples. One directory is one
scenario, and every scenario sets up all three chains, so start all three local
development nodes first:

```bash
bitcoind -regtest -txindex -rpcuser=bitcoin -rpcpassword=bitcoin -rpcport=18443
anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0
solana-test-validator --reset

cargo run -p payment-examples --example wallet      # create and import wallets
cargo run -p payment-examples --example transfer    # fund, send, balance, history
cargo run -p payment-examples --example indexing    # one Index, one sync loop
cargo run -p payment-examples --example watch_only  # observe an address, no key
cargo run -p payment-examples --example all_chains  # the composition root, in miniature
```

[`examples/README.md`](examples/README.md) lists all five scenarios, what each
one shows, and the nodes each needs.

## Documentation

- [`docs/SYSTEM_REQUIREMENTS.md`](docs/SYSTEM_REQUIREMENTS.md) defines canonical
  scope and acceptance criteria.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) defines ownership and dependency
  direction, starting with the crate map.
- [`docs/CHAIN_LAYOUT.md`](docs/CHAIN_LAYOUT.md) defines the one layout every
  chain crate follows, and what adding a chain costs.
- [`docs/INDEXING.md`](docs/INDEXING.md) defines synchronization and persistence
  semantics.
- [`docs/API.md`](docs/API.md) documents the public HTTP surface.

## Validation

Use locked dependencies:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --doc
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
sh scripts/check-chains.sh
linter check .
git diff --check
```

Tests use loopback RPC doubles and temporary databases. Never point tests or
examples at a funded key or public broadcast endpoint. `examples/` (see
`examples/README.md`) has one directory per scenario — `wallet`, `transfer`,
`indexing`, `watch_only`, and `all_chains` — all against local development
nodes.
