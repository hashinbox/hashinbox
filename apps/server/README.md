# payment-server: the composition root

## Why this crate exists

`payment-server` is the workspace's only executable. It reads one
configuration document — TOML for a `.toml` path, JSON for anything else — verifies every configured chain's RPC identity before opening
storage, builds each chain's indexer and wallet factories over one repository on
one SQLite file,
composes them into
one `indexing::Index` and one `wallets::Wallets<payment_api::AssetTag>`,
rebuilds the wallets it holds keys for from `wallets[]` and has the index watch
them, supervises one indexing task per chain and the submission work, and
serves the `payment_api` router until an ordered shutdown. One chain's indexing failure quarantines that chain
alone: the others keep syncing, `GET /v1/status` names the failure, and the
process neither exits nor stops answering.

It is the only crate that names a chain, and inside it only `src/chains.rs`
does (`scripts/check-chains.sh` check 8) — in **three lines**, one
`sdk_config::Registry::register` per chain. Everything after composition,
including this crate's `main`, uses the abstractions. Each chain knows how to
assemble itself — its own `Config<AssetTag>`, `connect` and `compose` live in
its crate (`docs/CHAIN_LAYOUT.md` section 4) — `sdk_config` hands each named
section to the chain that registered for it, and `wallets::Composition` holds
any number of them. Nothing here reads a chain's section, decides what its
keys mean, or prints anything about one.

## Concepts

| Concept | Where | Notes |
|---|---|---|
| configuration | `src/config.rs` | `bind`, `bearer_token_env`, `storage`, `chains`, startup `wallets`: this process's key store, one environment variable per key. `sdk_config::Document::read` is the reader; `chains` is an `sdk_config::Sections` this module never looks inside |
| storage | `src/storage.rs` | `Store` opens the one SQLite file; `Repository` is the one value over it that serves every network, because each operation names the network it is about. It is the `indexing::WatchStore` and `indexing::IndexStore` every index requires plus every consumer capability this server's chains read. It names a path, not a backend: there is one store, so there is nothing to select |
| which chain reads which section | `src/chains.rs` | one `registry.register::<chain_X::Config<AssetTag>>("x")` per chain. A fourth chain is a fourth line |
| composed chains | `sdk_config::Chains` | each chain's own `Config<AssetTag>` connects (identity check), and its verified node is a `wallets::Connection` that composes one `wallets::ComposedChain`: an index already mirroring its watch list to the store, that network's history reader and balance fold, factories under asset tags, and the `base::Units` that chain declares for its own assets |
| what an operator is told before anything connects | `sdk_config::Chains::notices` | each chain states what accepting its section commits the deployment to (Bitcoin's confirmation depth is the only one today); `main` prints them. A library crate must not write to standard output |
| composition | `wallets::Composition::verify_then_build` | the SDK's, not this binary's: it opens storage only after every identity check held, and returns one `Index`, one `Arc<dyn indexing::History>` and one `Arc<dyn indexing::Balances>` (each routed to the network that owns the holding, so no query is read through the index), one `Wallets` (factories only, no index handle and no id map), one `base::Units` (what an amount of each configured asset is, by `AssetId`) and one `Schedules` |
| what can sign | `src/main.rs`, `src/config.rs` | `payment_api::Signers`, filled from `wallets[]`; each entry is `from` the key, then `index.watch`, then `keep`, in that order (`docs/design/030 §8 (ex-029)` section 3.3) |
| offline tools | `src/rescan.rs` | `--rescan <chain>/<network>` empties one network. It connects no chain, starts no supervisor, and binds no listener |
| supervision | `src/submissions.rs`, `src/readiness.rs` | chain-neutral: `wallets::SubmissionRegistrar`, the `indexing::SyncMonitor` that logs each network transition and publishes `indexing::SyncHealth`. Shutdown order is seven consecutive statements at the end of `main`, where nothing can skip a stage |
| cadence | `wallets::Composition::schedules` | one `indexing::Schedule` per chain, from its own `cadence.poll_millis`; no chain is forced onto another's interval |
| readiness | `src/readiness.rs`, `src/main.rs` | `all(Ready \| Quarantined) && any(Ready)`; the listener binds once every network has settled (Ready once, or Quarantined). Forward indexing decides it: a network still owing history below its checkpoint is `Ready`, and the owed range is reported on `GET /v1/status` instead of holding the listener (`docs/design/024` §7) |
| offline rescan | `src/rescan.rs` | `--rescan <chain>/<network>` purges one network's indexing rows through `indexing::Purging::purge_network` and exits; no chain connects, no supervisor starts, no listener binds |

## Public API map

| Group | Items |
|---|---|
| run | `payment-server <config.toml>`; `payment-server --openapi` prints the OpenAPI document and exits; `payment-server <config.toml> --rescan <chain>/<network>` purges one network's indexing rows offline and exits |
| configuration | [`docs/API.md`](../../docs/API.md) §Configuration |
| library | none; the HTTP contract is the `payment-api` crate |

## Usage

```bash
export PAYMENT_API_TOKEN='replace-me'
cargo run --locked -p payment-server -- ./payments.toml
```

A minimal single-node configuration (one embedded file is the only storage):

```toml
bind = '127.0.0.1:8080'
bearer_token_env = 'PAYMENT_API_TOKEN'
storage = { path = '/var/lib/payment/payment.db' }

[chains.bitcoin]
network = 'regtest'
confirmation_depth = 6
rpc = { endpoints = ['http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443'] }
# Optional on every chain, one shape on every chain.
cadence = { poll_millis = 1000, batch_size = 100 }
```

Every chain's section has the same shape: its own keys, one `rpc`, one
optional `cadence`. The name of a section is the name that chain was
registered under in `src/chains.rs`; an unknown one is refused by name and
answered with the names this build does configure.

`confirmation_depth` is Bitcoin's alone and optional (default `6`, range
1..=1000): it is how many blocks below the node's tip this deployment commits,
because Bitcoin states no finality of its own. A smaller depth makes history
and spendable balance appear sooner; a reorg deeper than it quarantines that
one network for good, and the offline rescan below is the only way back.
Ethereum and Solana reject the key —
their nodes state the line themselves — and the retired `reorg_retention` is
rejected everywhere. The binary prints the Bitcoin line it runs at at startup,
and `GET /v1/status` reports it continuously as `checkpoint` against
`pending_block`. See [`docs/API.md`](../../docs/API.md#configuration).

Recovering one quarantined network whose cause was fixed out of band, with the
server running:

```bash
curl -X POST http://127.0.0.1:8080/v1/status/resume \
  -H "Authorization: Bearer $PAYMENT_API_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"network": {"chain": "bitcoin", "name": "mainnet"}}'
```

The composition root takes `SyncControl` from the supervisor before `run`
consumes it, so that one network's attempt count resets and its next pass runs
at once. Nothing durable changes and no other chain is touched; a cause that is
still unfixed fails again on that pass and says so on `GET /v1/status`.

Recovering one quarantined network the resume cannot fix -- `confirmed_reorg` --
with the server stopped:

```bash
cargo run --locked -p payment-server -- ./payments.toml --rescan bitcoin/mainnet
```

It removes that network's checkpoint, history, movements, block credits,
unmatched spends, live outputs and recorded watch progress, prints the counts,
and exits; it never touches the watch list or another network, and it binds no
listener, so the next ordinary start re-indexes the network from the earliest
registered address start position. A malformed
argument and a network this file does not configure are refused before storage
is opened. See [`docs/INDEXING.md`](../../docs/INDEXING.md) §The offline rescan.

Adding a chain means one new `sdk/chains/<chain>/src/config.rs` in that chain's
own crate — with its `impl wallets::Connection` and its `impl sdk_config::Chain`
beside the `Config` they belong to — plus **one `register` line** here (see
[`docs/CHAIN_LAYOUT.md`](../../docs/CHAIN_LAYOUT.md) section 4, "Plugging into
the composition root"). No route, wire model, `config.rs` or `main` changes.

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| api | `payment-api` | the router and wire models this binary serves |
| chains | `chain-bitcoin`, `chain-ethereum`, `chain-solana` | named in `src/chains.rs` only, once each |
| config | `sdk-config` | with feature `toml`: the one crate in the workspace that links a TOML parser |
| base, indexing, wallets, packages | `base`, `indexing`, `wallets`, `http-server`, `json-rpc` | the abstractions everything after composition uses; `indexing`'s default features bring its SQLite store and its supervisor |

Nothing depends on this crate. Design notes:
[`ARCHITECTURE.md`](../../ARCHITECTURE.md),
[`docs/SYSTEM_REQUIREMENTS.md`](../../docs/SYSTEM_REQUIREMENTS.md).
