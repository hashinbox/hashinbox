# 073 — Network tables: static config in SQL, typed SDK config

Status: proposed.

## Decisions

- Network data leaves `config.toml` for four tables: `network` (static),
  `network_asset` (static, native included), `network_endpoint` (nodes; as
  today) and `network_index` (dynamic indexing state).
- Chain-specific fields are **columns** (nullable, one group per chain,
  per-chain CHECKs). No JSON column: the point is to drop the document. ~10
  chain-only columns is cheap; SQL can enforce them; diesel maps them to
  typed fields; `UPDATE network SET enabled = 0` works by hand.
- Every setting is NOT NULL and seeded explicitly (except the "no limit" /
  "show all" ones). The table shows what runs; no hidden SDK defaults.
- One switch: `network.enabled`. The server builds and runs the enabled
  networks only.
- `network`, `network_asset`: a change needs a restart. `network_endpoint`:
  live through the API (as today). `network_index`: written every indexer
  step, read live.
- The SDK gets typed configs through `Builder::with_network`, which already
  exists. The only SDK change is deleting the document path (`Document`,
  `with_chain`, `with_config`).
- `payment-server migrate` runs `init.sql`, then `seed.sql`, in one write.

## Tables

```sql
CREATE TABLE network (
    chain            TEXT    NOT NULL CHECK (chain IN ('bitcoin', 'ethereum', 'solana')),
    network          TEXT    NOT NULL,           -- slug; no dot
    testnet          INTEGER NOT NULL CHECK (testnet IN (0, 1)),
    enabled          INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    confirmations    INTEGER NOT NULL CHECK (confirmations >= 0),
    interval_ms      INTEGER NOT NULL CHECK (interval_ms > 0),   -- indexer wake ("block time")
    fee_ttl_ms       INTEGER NOT NULL CHECK (fee_ttl_ms > 0),
    pending_ttl_ms   INTEGER NOT NULL CHECK (pending_ttl_ms > 0),
    max_history_days INTEGER CHECK (max_history_days > 0),       -- NULL: no limit
    explorer_address     TEXT,  -- '{address}' filled in; NULL: no explorer
    explorer_transaction TEXT,  -- '{tx}' filled in
    -- ethereum
    chain_id                 INTEGER CHECK (chain_id > 0),
    gas_margin_bps           INTEGER CHECK (gas_margin_bps >= 0),
    max_gas_limit            INTEGER CHECK (max_gas_limit > 0),
    max_fee_per_gas          TEXT,  -- wei, exact decimal (u128 > i64)
    max_priority_fee_per_gas TEXT,
    max_total_fee            TEXT,
    -- solana
    genesis_hash      TEXT,         -- Base58
    -- bitcoin
    address_type      TEXT CHECK (address_type IN ('segwit_v0', 'taproot')),
    fee_target_blocks INTEGER CHECK (fee_target_blocks > 0),
    fee_floor         INTEGER CHECK (fee_floor > 0),     -- sat per 1,000 vB
    PRIMARY KEY (chain, network),
    CHECK ((explorer_address IS NULL) = (explorer_transaction IS NULL)),
    CHECK (IIF(chain = 'ethereum',
        chain_id IS NOT NULL AND gas_margin_bps IS NOT NULL AND max_gas_limit IS NOT NULL
        AND max_fee_per_gas IS NOT NULL AND max_priority_fee_per_gas IS NOT NULL
        AND max_total_fee IS NOT NULL,
        COALESCE(chain_id, gas_margin_bps, max_gas_limit, max_fee_per_gas,
                 max_priority_fee_per_gas, max_total_fee) IS NULL)),
    CHECK (IIF(chain = 'solana', genesis_hash IS NOT NULL, genesis_hash IS NULL)),
    CHECK (IIF(chain = 'bitcoin',
        address_type IS NOT NULL AND fee_target_blocks IS NOT NULL AND fee_floor IS NOT NULL
        AND network IN ('mainnet', 'testnet3', 'testnet4', 'signet', 'regtest')
        AND confirmations < 1000,
        COALESCE(address_type, fee_target_blocks, fee_floor) IS NULL)),
    CHECK (COALESCE(max_fee_per_gas, '0') NOT GLOB '*[^0-9]*'
       AND COALESCE(max_priority_fee_per_gas, '0') NOT GLOB '*[^0-9]*'
       AND COALESCE(max_total_fee, '0') NOT GLOB '*[^0-9]*')
) WITHOUT ROWID;

-- One asset of a network: its native currency (kind 'native', exactly one
-- per network, no contract) or an allowlisted token (kind 'contract': ERC-20
-- contract, canonical lowercase 0x hex; SPL mint, Base58).
-- id: "{chain}.{network}.{asset}", asset = the SDK asset id
-- (COALESCE(contract, 'native')).
CREATE TABLE network_asset (
    id               TEXT    NOT NULL PRIMARY KEY,
    chain            TEXT    NOT NULL,
    network          TEXT    NOT NULL,
    kind             TEXT    NOT NULL CHECK (kind IN ('native', 'contract')),
    contract         TEXT,                     -- NULL exactly for native
    decimals         INTEGER NOT NULL CHECK (decimals BETWEEN 0 AND 255),
    ticker           TEXT    NOT NULL CHECK (length(ticker) BETWEEN 1 AND 16),
    display_decimals INTEGER CHECK (display_decimals BETWEEN 0 AND decimals), -- NULL: all
    FOREIGN KEY (chain, network) REFERENCES network (chain, network) ON DELETE CASCADE,
    CHECK ((kind = 'native') = (contract IS NULL)),
    CHECK (id = chain || '.' || network || '.' || COALESCE(contract, 'native')),
    CHECK (kind = 'native' OR chain IN ('ethereum', 'solana')),
    -- The native row repeats the chain's constants; the SDK takes none of them.
    CHECK (kind = 'contract'
        OR (chain = 'bitcoin'  AND decimals = 8  AND ticker = 'BTC')
        OR (chain = 'ethereum' AND decimals = 18 AND ticker = 'ETH')
        OR (chain = 'solana'   AND decimals = 9  AND ticker = 'SOL')),
    CHECK (chain <> 'ethereum' OR contract IS NULL
        OR (contract GLOB '0x*' AND length(contract) = 42 AND contract = lower(contract)))
);
CREATE UNIQUE INDEX network_asset_native ON network_asset (chain, network)
    WHERE kind = 'native';

-- Unchanged: one row per network, its nodes (JSON list), failover order.
-- New: FOREIGN KEY (chain, network) REFERENCES network ON DELETE CASCADE.
CREATE TABLE network_endpoint (chain, network, nodes, PRIMARY KEY (chain, network));

CREATE TABLE network_index (
    chain      TEXT    NOT NULL,
    network    TEXT    NOT NULL,
    tip        INTEGER,          -- node's produced tip at the last step; NULL: none read
    confirmed  INTEGER,          -- tip - network.confirmations
    indexed    INTEGER,          -- last kept block's position (Stats.last); NULL: none
    checked_at INTEGER,          -- Unix ms of the last step whose tip moved
    started_at INTEGER NOT NULL, -- Unix ms the counters below count from (process start)
    blocks     INTEGER NOT NULL DEFAULT 0, -- kept since started_at
    reorgs     INTEGER NOT NULL DEFAULT 0,
    watched    INTEGER NOT NULL DEFAULT 0,
    failures   INTEGER NOT NULL DEFAULT 0,
    error      TEXT,             -- last failure; cleared by the next kept block
    PRIMARY KEY (chain, network),
    FOREIGN KEY (chain, network) REFERENCES network (chain, network) ON DELETE CASCADE,
    CHECK ((tip IS NULL) = (confirmed IS NULL)),
    CHECK (confirmed IS NULL OR confirmed <= tip)
) WITHOUT ROWID;
```

Views: `wallet_index_with_balance` joins `network_index` instead of
`network` (same columns `tip`, `confirmed`). `network_with_endpoints`
becomes `network_with_index`: `network` LEFT JOIN `network_index` LEFT JOIN
`network_endpoint`.

Bitcoin `confirmations` is the service's `confirmations`; the server sets
`confirmation_depth = confirmations + 1`, so `< 1000` keeps the depth in
the SDK's `1..=1000`.

## Data flow

Startup (`serve`):

1. Read `config.toml`: bind, auth, storage, brand. Open the db.
2. One read: `network WHERE enabled = 1` (`network::db::find_enabled`),
   their `network_asset` rows (`network_asset::db::find_all`), and
   `endpoint::db::find_all` (as today). Each model owns its query.
3. `main.rs` builds one typed chain config per row (see below). It refuses
   to start, naming the network, when an enabled network has no endpoint
   row or no native asset row.
4. `Payments::builder().with_network(..)` once per network, then
   `.with_storage(..).build()`, as today.
5. `Index::start` upserts each built network's `network_index` row (see
   below) and queues its index job, as today.

Changes:

| Table | Changed by | Takes effect |
|---|---|---|
| `network`, `network_asset` | SQL by hand (no API) | restart |
| `network_endpoint` | `/v1/networks/{c}/{n}/endpoints` routes: stored, then `Swap::replace` | live (as today); a hand SQL edit needs a restart |
| `network_index` | the index worker, every step | live: read by every balance and networks read |

No hot reload of `network`. Building a service connects to and verifies
nodes, and `Payments` is an immutable map. Reload is a restart.

## Typed SDK config

Today: `payments::Document { networks: Vec<Map<String, Value>> }`,
`Builder::with_chain::<C>(name)` registers a serde parser per chain name,
and `Builder::with_config(document)` hands the sections over.
`main::stored_endpoints` rewrites each section's `endpoints` from
`network_endpoint` before parsing.

Proposed: no new SDK API. `Builder::with_network(impl base::Config)` already
takes a typed config (the server's tests already use it). The server
builds each chain's config:

| Chain | Built as | From |
|---|---|---|
| ethereum | `chain_ethereum::Config::new(Network::new(chain_id, network), endpoints)`, then set `limits: FeeLimits {..}` and the ttl, interval, confirmations, history fields | `network` row; `tokens` = contract rows (`contract.parse::<alloy_primitives::Address>()`, decimals, ticker, display_decimals); `display_decimals` = native row's |
| solana | `chain_solana::Config::new(Network::new(genesis_hash.parse()?, network), endpoints)`, same fields | the same; mint = `contract.parse::<solana_address::Address>()`, refused unless `mint.to_string() == contract` |
| bitcoin | `chain_bitcoin::Config::new(network, endpoints)`, with `confirmation_depth = confirmations + 1`, address_type, fee_target_blocks, fee_floor, ttls, interval, history | slug and address_type matched in `main.rs` (5 and 2 arms); `display_decimals` = native row's |

Endpoints come from the stored nodes (`models::RpcEndpoints::nodes()`, as
`install` already uses them). The builders live in `main.rs` (the only
module that names a chain crate), one `fn` per chain, ~25 lines each.

SDK files changed (deletions only):

- `sdk/payments/src/config.rs`: deleted (`Document`, its tests).
- `sdk/payments/src/payments.rs`: delete the `Parser` type, the
  `Builder.parsers` and `Builder.document` fields, `with_chain`,
  `with_config` and the sections loop in `build`.
- `sdk/payments/src/lib.rs`: drop `pub use config::Document`.
- `sdk/payments/Cargo.toml`: drop the `toml` feature and dependency.
- `sdk/payments/README.md`: the document section becomes `with_network`.
- `sdk/chains/*`: no change. Configs stay `Deserialize` for third parties
  that want a file.

The server adds `solana-address` (workspace version `=2.2.0`) to
`apps/server/Cargo.toml`. It already has `alloy-primitives`.

## network_index

Who writes each column:

- `Index::start` (once per enabled network, at startup): inserts the row
  if missing, and sets `started_at = now`, `blocks = reorgs = watched =
  failures = 0`, `error = NULL`. It keeps `tip`, `confirmed`, `indexed`
  and `checked_at`.
- The index worker, after every step (`network_index::db::save`, which
  replaces `network::db::save`): `tip`, `confirmed` (`tip -
  service.confirmations`), `indexed` (COALESCE with the stored value),
  `checked_at` (only when the tip moved), and `blocks`, `reorgs`,
  `watched`, `failures`, `error` from `indexer.stats()`. It now writes on a
  failed step too, so `failures` and `error` land. `tip` stays as stored
  when the step read none.
- Nobody else writes it.

The counters count since the process started (`started_at`), as
`Stats` does. All-time counts are already rows (`block`, archived blocks,
`wallet_index`) that `usecase::stats` counts.

What it drives:

- Every balance: `wallet_index_with_balance.finality_at` / `behind_tip`
  read `network_index.confirmed` / `tip`.
- `/health` and `/v1/networks` indexing fields (below).
- Not which networks run. `network.enabled` decides that, and the row
  follows (created at startup for each enabled network). Disabling keeps
  the row frozen; re-enabling resets its counters.

In memory: `base::Stats` and `Payments::stats()` stay in the SDK
unchanged. The worker reads them to write the row. The server stops
reading them anywhere else. `models/indexing.rs` (`Indexing`) is deleted
and replaced by `models/network_index.rs` (`NetworkIndex`): the row, its
statements, and the API shape both routes return.

## seed.sql

Embedded like `init.sql`. `Db::migrate` runs `init.sql` then `seed.sql`
in one `Db::write`. It runs on a new file only, so it uses plain `INSERT`.

```sql
-- network: today's four config.toml networks, all enabled.
--   ethereum sepolia  testnet=1 chain_id=11155111     explorer sepolia.etherscan.io
--   ethereum mainnet  testnet=0 chain_id=1            explorer etherscan.io
--   solana   devnet   testnet=1 genesis EtWTRABZ…ZBG  explorer explorer.solana.com ?cluster=devnet
--   solana   mainnet  testnet=0 genesis 5eykt4Us…N9d  explorer explorer.solana.com
--   every one: max_history_days 30.
--   ethereum: confirmations 12, interval_ms 12000, fee_ttl_ms 12000,
--     pending_ttl_ms 3600000, gas_margin_bps 2000, max_gas_limit 30000000,
--     max_fee_per_gas '1000000000000', max_priority_fee_per_gas '100000000000',
--     max_total_fee '10000000000000000000'   (today's SDK defaults, stated)
--   solana: confirmations 32, interval_ms 1000, fee_ttl_ms 5000, pending_ttl_ms 120000
-- network_asset:
--   native: ETH/18 display 6 (both ethereum), SOL/9 display 4 (both solana)
--   sepolia USDC 0x1c7d…7238 /6 d2;  ethereum mainnet USDC 0xa0b8…eb48, USDT 0xdac1…1ec7 /6 d2
--   devnet  USDC 4zMMC9…DncDU /6 d2; solana mainnet  USDC EPjFWd…Dt1v, USDT Es9vMF…wNYB /6 d2
-- network_endpoint (nodes JSON lists):
--   ethereum.sepolia ["https://ethereum-sepolia-rpc.publicnode.com"]
--   ethereum.mainnet ["https://ethereum-rpc.publicnode.com"]
--   solana.devnet    ["https://api.devnet.solana.com"]
--   solana.mainnet   ["https://api.mainnet-beta.solana.com"]
-- network_index: none (Index::start creates the rows).
```

Server tests run `migrate`, so they get the seed. The test harness builds
a fake `bitcoin/regtest` through `with_network`, so it also inserts one
`bitcoin.regtest` `network` row (for the `network_index` foreign key).

## config.toml after

```toml
bind = '127.0.0.1:8080'
storage = { path = '.data/payment.db' }
# bearer_token_env = '…'        (optional, as today)
# tls_terminated_upstream = true (optional, as today)
[ui]
brand = 'payments · dev'
```

`PAYMENT_UI_API_BASE` stays an environment variable. `Config` keeps
`bind`, `bearer_token_env`, `tls_terminated_upstream`, `storage` and
`ui.brand`.

## API and UI

- `GET /v1/networks`: lists enabled networks, as today. Each item:
  `network`, `testnet` (new), `confirmations` (new), `explorer` (from the
  row), `max_history_days` (from the row), `endpoints`, `tip_live`,
  `tip_error`, and `index: NetworkIndex` {`tip`, `confirmed`, `indexed`,
  `checked_at`, `started_at`, `blocks`, `reorgs`, `watched`, `failures`,
  `error`}. The flat `tip`, `confirmed`, `indexed`, `checked_at` and
  `index_error` move under `index`. One shape, shared with `/health`.
- `GET /health`: `networks[].indexing` becomes `networks[].index`
  (`NetworkIndex`). It reads `network_index` (one SQLite read, no RPC). It
  is ready when at least one network is enabled, none has `error`, and the
  read succeeded. Not ready otherwise (503).
- `GET /v1/assets`: unchanged. It reads the SDK's assets, which equal the
  `network_asset` rows by construction.
- No new CRUD now. Enabling a network or adding a token is `UPDATE` or
  `INSERT`, then a restart.
- UI: regenerate `api/schema.d.ts`. Read `index.*` in
  `pages/networks/page.tsx`, `pages/networks/_components/nodes.tsx`,
  `pages/wallets/_components/{wallet,search,rail}.tsx`, `watching.ts` and
  `pages/jobs/page.tsx` where they read the flat fields. `explorers.ts` is
  unchanged (the layout still fills it from `/v1/networks`).

## Deleted

- `sdk/payments`: `config.rs` (119), the builder's document path (~40),
  the `toml` feature, and the README section.
- `apps/server/src/config.rs`: the `payments` field and `Ui.explorers`.
- `apps/server/src/main.rs`: `stored_endpoints` (~35), the three
  `with_chain` calls and `with_config`.
- `models/explorer.rs`: `Explorers` (the map). `Explorer` stays, built from
  the row's two columns. `AppState.explorers` and its 3 test sites go too.
- `models/indexing.rs` (40). `network::db::create` and `save` move to
  `network_index`.
- `init.sql`: the old `network` table and the `network_with_endpoints`
  view.
- `config.toml`: `[ui.explorers.*]` and `[[payments.networks]]`, with
  their comments (~70 lines).
- `apps/server/README.md`: the `payments` document section, replaced by
  `seed.sql`.

Honest count: Rust about −250 (SDK −170, server −80) and about +300
(`main.rs` builders +90, `network_asset.rs` +60, `network_index.rs` +90,
`network.rs` reshaped +40, routes +20). That is roughly even. SQL is about
+150 (three tables with CHECKs, about 70 lines of seed). TOML is −70. The
gain is typed config with one source of truth, not fewer lines.

## Open questions

1. Delete `payments::Document`, `with_chain` and `with_config` outright?
   **Recommend yes.** The server is the only user; third parties can still
   serde-read a chain `Config`.
2. Should `/health` read `network_index` (a db read) instead of memory?
   **Recommend yes.** It gives one shape and one source, and an unreadable
   db means the server is not ready.
3. Should `/v1/networks` list disabled networks too (with `enabled`)?
   **Recommend not now.** List them when an enable route exists.
4. Should the counters (`blocks`, `reorgs`, …) count since process start
   or all time? **Recommend since start (`started_at`).** All-time counts
   are already rows.
5. Is "block time" just `interval_ms`, or a separate `block_time_ms`?
   **Recommend `interval_ms` only.** Nothing else would read a separate
   field.
6. Should the seed enable mainnets as well as testnets? **Recommend yes,
   all four.** That is today's behavior; one `UPDATE` disables one.
