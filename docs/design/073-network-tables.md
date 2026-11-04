# 073 — Networks are tables

## Decisions

- Networks live in the database, not a config file. `config.toml` is gone;
  the server takes `--db`, `--bind`, `--bearer-token-env`,
  `--tls-terminated-upstream`, `--brand` (each with a `PAYMENT_*` env fallback).
- Four tables: `network` (static facts), `network_asset`, `network_endpoint`,
  `network_index` (indexer state). Only PK, NOT NULL and FK constraints.
- Chain-specific tuning is one JSON column, `network.settings`; each chain
  reads its own keys with today's defaults. No per-chain columns.
- `network_index` drives indexing: the server indexes exactly the networks
  that have a row, and adds one for each `enabled` network at start (existing
  rows are kept). Payments is built from those networks only.
- The SDK gets one typed, chain-neutral config, `base::NetworkConfig`, that
  mirrors the rows. It lives in `base` because both the chain crates (which
  build from it) and `payments` (which takes it) depend on `base`, and
  neither depends on the other. The server maps rows → `NetworkConfig` with
  plain `From`/`TryFrom`, with no per-chain code.
- `network_index` counters (`blocks`, `reorgs`, `watched`, `failures`) are
  all-time: each step adds what the in-memory `base::Stats` grew by since the
  worker last wrote. `error` is the last failure, cleared by the next kept block.

## Tables

```sql
CREATE TABLE network (
    chain TEXT NOT NULL, network TEXT NOT NULL,
    chain_id TEXT,                     -- ETH chain id, SOL genesis hash; NULL: name is enough
    testnet INTEGER NOT NULL, enabled INTEGER NOT NULL,
    block_time_ms INTEGER NOT NULL,    -- the chain's real block time
    confirmations INTEGER NOT NULL,
    max_history_days INTEGER,          -- NULL: no limit
    explorer_address TEXT, explorer_transaction TEXT,  -- `{address}` / `{tx}` templates
    settings TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (chain, network));

CREATE TABLE network_asset (
    chain TEXT NOT NULL, network TEXT NOT NULL,
    asset TEXT NOT NULL,               -- 'native' or the contract/mint
    kind TEXT NOT NULL,                -- 'native' | 'contract'
    contract TEXT,                     -- NULL for native
    ticker TEXT NOT NULL, decimals INTEGER NOT NULL,
    display_decimals INTEGER,          -- NULL: all
    PRIMARY KEY (chain, network, asset),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network));

CREATE TABLE network_endpoint (
    chain TEXT NOT NULL, network TEXT NOT NULL,
    nodes TEXT NOT NULL,               -- JSON list of rpc::Endpoint documents
    PRIMARY KEY (chain, network),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network)) WITHOUT ROWID;

CREATE TABLE network_index (
    chain TEXT NOT NULL, network TEXT NOT NULL,
    tip INTEGER, confirmed INTEGER, indexed INTEGER, checked_at INTEGER,
    blocks INTEGER NOT NULL DEFAULT 0, reorgs INTEGER NOT NULL DEFAULT 0,
    watched INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    PRIMARY KEY (chain, network),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network));
```

`wallet_index_with_balance` joins `network_index` for `tip` and `confirmed`.
No other view reads these tables; `network_with_endpoints` is gone.

## Typed config (sdk/base)

```rust
pub struct NetworkConfig {
    pub network: Network,              // chain + name
    pub chain_id: Option<String>,
    pub testnet: bool,
    pub block_time: Duration,
    pub confirmations: u64,
    pub max_history: Option<Duration>,
    pub explorer_address: Option<String>,
    pub explorer_transaction: Option<String>,
    pub settings: serde_json::Value,
    pub assets: Vec<AssetConfig>,      // { asset, kind: AssetKind, contract, ticker, decimals, display_decimals }
    pub endpoints: Vec<rpc::Endpoint>,
}
```

- Each chain: `impl TryFrom<&base::NetworkConfig> for Config`. A private
  `Settings` (serde, `default`, `deny_unknown_fields`) holds the chain's knobs
  and their defaults (also used by `Config::new`). Bitcoin: `address_type`,
  `fee_target_blocks`, `fee_floor`, `fee_ttl_ms`, `pending_ttl_ms`,
  `interval_ms`; `confirmation_depth = confirmations + 1`. Ethereum: `limits`,
  `fee_ttl_ms`, `pending_ttl_ms`, `interval_ms`. Solana: `fee_ttl_ms`,
  `pending_ttl_ms`, `interval_ms`. Each checks the built network equals the
  config's and the native decimals match the chain.
- Chain `Config`s no longer derive `Deserialize`; `max_history_days` became
  `max_history: Option<Duration>`.
- `payments::Builder`: `with_chain::<C>(Chain)` registers a chain
  (`C: TryFrom<&NetworkConfig>`), `with_network_config(NetworkConfig)` adds a
  network; `with_network(impl Config)` stays for configs built in code.

## Server

- `models/network.rs`: `Network` (row + `assets` + `endpoints` + `index` +
  `tip_live`/`tip_error`), `db::find_all` (plain reads of the four tables),
  `From<&Network> for base::NetworkConfig`.
- `models/network_asset.rs`, `models/network_index.rs` (`NetworkIndex`,
  `step`, `db::{create_enabled, save, find_all}`).
- Start: `create_enabled`, read networks, hand those with an index row to
  the builder; the index worker enqueues a job per `network_index` row.
- `GET /v1/networks`: every `network` row as stored (column names), with
  `assets`, `endpoints`, `index` (null: not indexed), `index_job_reason`
  (the index job's `q.reason`), `tip_live`, `tip_error`.
- `GET /health`: same shape as before; `indexing` is the `network_index` row;
  not ready while an index job fails (failed, or going again with a reason).

## Seed

`apps/server/seed.sql`, run by `Db::migrate` after `init.sql` in one write:
ethereum sepolia (11155111) and mainnet (1), solana devnet and mainnet
(genesis hashes); all enabled, 30 history days, ETH 12 s / 12 confirmations,
SOL 400 ms / 32 confirmations, explorers; native ETH (18, shows 6), SOL (9,
shows 4); USDC (sepolia, devnet, both mainnets) and USDT (both mainnets),
6 decimals, shows 2; one public endpoint each. `settings` is `{}`: the old
file set no non-default knob. Tests insert their own `bitcoin/regtest` rows.

## Deleted

- `config.toml`, the TOML/JSON reading in `apps/server/src/config.rs`, the
  `toml` dependency, `[ui.explorers]` and `models/explorer.rs`.
- `models/indexing.rs` (its fields now live in `NetworkIndex`).
- The old dynamic `network` table, the `network_with_endpoints` view, and
  `main.rs`'s endpoint seeding from the document.
- `payments::Document`, `with_config`, the document-reading `with_chain`, the
  `toml` feature, and `sdk/payments/src/config.rs`.
