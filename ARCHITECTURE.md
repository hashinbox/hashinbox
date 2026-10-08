# Architecture

Current owners and invariants; exact APIs live in the crate READMEs and source.
Product acceptance lives in [SYSTEM_REQUIREMENTS.md](docs/SYSTEM_REQUIREMENTS.md).

## Crates

| Crate | Path | Purpose | Layer | README |
|---|---|---|---|---|
| `tasks` | `packages/tasks` | Owned Tokio tasks, closed admission and graceful draining; no SDK dependencies | packages | [README](packages/tasks/README.md) |
| `crypto` | `packages/crypto` | Secret key material, secp256k1 and Ed25519 keys, ECDSA/Schnorr/Ed25519 signing, the signing request/result vocabulary, and the in-process key pair every chain signs through | packages | [README](packages/crypto/README.md) |
| `decimal` | `packages/decimal` | Exact base-10 amounts: canonical positional text, numeric ordering, atomic-unit conversion | packages | [README](packages/decimal/README.md) |
| `json-rpc` | `packages/json-rpc` | Bounded JSON-RPC transport: one URL per node; retry and failover are decorators | packages | [README](packages/json-rpc/README.md) |
| `http-server` | `packages/http-server` | Axum server hardening: bearer auth, transport security, limits; application-supplied health routes | packages | [README](packages/http-server/README.md) |
| `sdk-config` | `sdk/config` | The configuration document and the registry that hands each named section to the chain that registered for it; `Rpc`, `Repetition`, `Cadence` as plain data; `Composition` of every registered chain's factories, index and read handles; TOML behind feature `toml` (off by default). It names no chain crate | config | [README](sdk/config/README.md) |
| `base` | `sdk/base` | Chain-neutral values and capabilities: address, network, asset, amount, block, sign, broadcast | base | [README](sdk/base/README.md) |
| `indexing` | `sdk/indexing` | Append-only, chain-neutral sync into history, checkpoint, and outputs, with pending blocks in memory; narrow store ports for live outputs and transaction lookup; optional SQLite/memory repositories and Tokio supervision | indexing | [README](sdk/indexing/README.md) |
| `wallets` | `sdk/wallets` | Wallet factory contract and the `Wallets` registry: wallet capabilities, sends and batches; no reads | wallets | [README](sdk/wallets/README.md) |
| `chain-bitcoin` | `sdk/chains/bitcoin` | Bitcoin: RPC, blocks, UTXO wallets, send pipeline | chains | [README](sdk/chains/bitcoin/README.md) |
| `chain-ethereum` | `sdk/chains/ethereum` | Ethereum: RPC, blocks, ETH and ERC-20 wallets, send pipeline | chains | [README](sdk/chains/ethereum/README.md) |
| `chain-solana` | `sdk/chains/solana` | Solana: RPC, finalized blocks, SOL wallets, send pipeline | chains | [README](sdk/chains/solana/README.md) |
| `payment-server` | `apps/server` | The one application, a binary with no library target, in three layers: models (`src/models/`, the nouns endpoints and tables share, with their wire form and, for wallets and RPC endpoints, their SQL on the one pool), the API (`src/api/`, one file per endpoint group, the router, OpenAPI root and shared error; handler state in `src/state.rs`), and composition (`main.rs` with `config.rs`, `chains.rs`, `storage.rs`, `operator.rs`, `ui.rs`) over `wallets` and `indexing` | server | [README](apps/server/README.md) |
| `payment-examples` | `examples` | Runnable scenarios: one directory per scenario, every chain inside it as setup | examples | [README](examples/README.md) |


## Boundaries

- Packages import packages, never SDK or applications. `base` imports no RPC,
  indexing, wallets or concrete chain. Concrete chains do not import one another.
- `apps/server` is the only application binary. Only its `src/chains.rs` selects
  concrete chains; `src/api/`, `src/asset.rs` and `src/state.rs` name neither chains,
  configuration parsing nor storage backends; `src/models/` names no HTTP framework,
  route or chain, but owns its serde/utoipa wire form. Nothing depends on the server.
- Each chain owns its protocol, native RPC, parsing, signing, finality and send
  pipeline. `native/` knows no SDK indexing/wallet model. Adapters translate at
  that boundary; shared values do not become a universal transaction/RPC model.
- Configuration owns documents, endpoint construction and chain registration.
  Each chain interprets its own section. The server composes every section once at
  startup; a section whose node fails verification is skipped, the rest serve. Direct
  Rust construction remains available without a parser.
- Wallet factories own construction and validation; `Wallets` collects factories,
  not wallet instances, IDs or secrets. The application owns custody and its
  signing map. Display units are keyed by exact `(Network, AssetId)`.
- Indexing owns observation and scanning. Repositories own persisted facts and
  queries. Continuous execution is optional; callers can drive bounded passes.

### Dependency direction

```text
server/examples -> chains, config, wallets, indexing, base, packages
chains          -> config, wallets, indexing, base, packages
config          -> wallets, indexing, base, json-rpc
wallets         -> base, crypto (+ optional tasks/Tokio)
indexing        -> base, crypto (+ optional sqlx/Tokio)
base            -> crypto, decimal
```

`wallets` and `indexing` never name each other; `config` composes both
(`sdk_config::Composition`). `linter.toml` and `scripts/check-chains.sh` (check
14) enforce crate and module boundaries.
Within `indexing/src/storage/`, each directory is one backend with `mod.rs`;
shared contracts/models are files directly under `storage/`. The engine and
shared storage code never import backend internals. SQLite and memory are
independently feature-gated, shipped implementations. Contract suites live in
`indexing/tests/`; memory is useful beyond testing. The one scripted chain
double is `indexing::testing` (feature `testing`, dev-dependencies only).

## Indexing and repositories

One caller-supplied repository serves all networks, including third-party
implementations. Its required contract is `WatchStore + IndexStore`; balance,
history, outputs and network purge are optional consumer capabilities.

`Indexer` owns one network's watches, pending `Blocks`, chain adapter and
repository reference. `Index` routes across them. Native-block caching is
separate from validated facts. The pass is a procedure over this state, not
another state owner. Chain interpreters emit facts, never SQL or stored records.

Block stages directly embed `base::Block<T>` alongside indexing output changes
and credits. Transaction stages directly embed `indexing::Transaction` for identity,
movements and fee, retaining their own status and inclusion order. Shared transaction
facts stay in indexing; native chain transactions keep their protocol-specific models.

```mermaid
sequenceDiagram
    participant App as Application
    participant Index
    participant I as Network Indexer
    participant C as Chain adapter
    participant R as Shared Repository
    App->>Index: watch(identity, address, asset, birthday)
    Index->>R: add(watch)
    R-->>Index: Durable registration
    Index->>I: Publish watch to scanning
    App->>Index: sync()
    Index->>I: One bounded pass
    I->>C: Fetch and interpret required blocks
    C-->>I: Validated facts
    I->>R: commit(facts, expected state, progress)
    Note over R: One atomic transaction
    R-->>I: Committed
    App->>R: Optional balance/history/output query
    R-->>App: Confirmed result, checkpoint, blocks behind
```

A `Watch` carries no key; the store's `wallet` row may hold a label and key via
`WalletRegistry`, and scanning never reads them. Persist a watch before publishing
it to the hot set; admission coordinates observation with active passes. Unwatch
archives the row, not previously indexed facts.

Per-watch progress proves contiguous inspection, including blocks with no matching
activity. Resume after saved progress or at the birthday. New watches do not reset
old ones. Behind watches catch up from their lowest required position using shared
fetches; they join forward selection only when caught up. Forward work gets the
bounded pass budget first. Reads must expose incomplete catch-up honestly.

Native position controls traversal, birthdays and restart; produced height controls
confirmation. Solana slots may be sparse. Block references retain hash and paired
parent position/hash; verify persisted hashes before resuming.

Each chain supplies its confirmed boundary: Bitcoin configured depth, Ethereum
finalized block, Solana rooted block. Commit only confirmed facts. Pending facts
stay in memory; a pending reorg truncates them without storage rollback. A confirmed
reorg stops that network for explicit offline recovery.

One commit atomically persists facts, output changes, checkpoint and affected watch
progress. Expected-state checks reject conflicting work; identical retries are
harmless. A cancelled or failed await can have committed: reconcile uncertain
outcomes. Historical reinspection uses the same commit, preserving other watches
and previously recorded facts. Output spend evidence prevents resurrection during
historical replay. No rollback journal or raw-block archive is needed.

SQLite stores normalized transactions, address associations, balance movements,
whole-life outputs, watches/progress and network checkpoints. Transactionless credits
have no invented transaction identity. Watched signed totals are maintained inside
the commit; queries use exact atomic arithmetic, never SQLite integer sums over
u256 text.
Each query reads its rows, checkpoint and how far its inspection is behind from one snapshot.

Balance and history reads belong to the application (051): `apps/server` owns
their SQL over indexing's tables and completes them with each network's pending
blocks (`indexing::Pending`); Bitcoin's coin selection (`chain_bitcoin::sender::Spendable`)
and Solana's absence proof (`chain_solana::sender::IndexPresence`) do the same over
`OutputStore` and `TransactionStore`. Every such read samples pending blocks
before the store read and narrows them using that answer's checkpoint. A commit
must never omit or count a block twice. History preserves all stable movements;
paging during backfill must not promise a complete export.

Network purge removes only that network's indexing facts and derived claims, offline or
online with that network paused and its admissions blocked; reads stay honest from the
store's own snapshot (`Incomplete`, spendable outputs refused) until passes commit again.
It preserves wallet rows (identities, birthdays, labels, keys, archive state),
application tables and other networks.
The next run, or the next pass after `Indexing::reset`, rebuilds from those birthdays.
Never run purge alongside a live writer of that network.

## Sending and custody

```text
request -> chain preflight/reservation -> native unsigned transaction
        -> injected Signer -> verify signature -> exact signed bytes
        -> chain outbox/broadcast -> indexing establishes confirmation
```

`Authorship` is optional: receive-only assets cannot obtain sending capabilities.
Solana SPL remains receive-only. Batches are non-empty, ordered and validated before
broadcast. Bitcoin can combine sources with deterministic fees and per-source
change/signatures. Ethereum preserves consecutive source nonces and accepted-prefix
reporting. Solana preserves source reservations, Memo identities, context floors,
blockhash lifetimes and complete finalized-history recovery. Preserve distinct native
stages where invariants differ: Bitcoin parsed inputs can lack signing prevouts,
reservations can outlive their signed transaction, and Solana wire values require
context and canonical-encoding validation before becoming domain values.

Each chain owns ambiguity and exact-byte replay. RPC acceptance means submitted;
an uncertain failure is not proof of a drop. Transport decorators must honor
`Request::once`; no Solana call is repeated, and like Bitcoin and Ethereum, Solana
broadcasts a submission to every configured node, each exactly once. Startup
checks exact chain identity, Ethereum tracing/token support and Solana's executable
Memo program before composition succeeds.

Wallets live in one table: indexing's `wallet` row, which scanning reads as a
watch (its active rows only, `archived_at IS NULL`) and an application reads
with its optional label and plaintext key through `WalletRegistry`, a capability
both shipped stores implement; the server keeps no wallet table. Creation files the
row first, archived and never observed (`register`), so the key is durable before
any observation; `index.watch` then reactivates it. An interrupted creation leaves an
archived, never-observed row. Archiving (`unwatch`/`forget`) clears the row's
progress and total and keeps its label, key, birthday and indexed facts; the pair
stays reserved for its id. Archiving is one-way: the API refuses to create an
archived wallet's address again and drops its signer. Boot rebuilds a signer for every active, keyed wallet of a served
network and configured asset; the index restores its watches from the same rows.
Routes return public identity, never keys. Key buffers use zeroizing memory; no
encryption or passphrase is required.

Outbox state remains in-process. Durable keys do not make outgoing logical payments
crash-safe: require one active writer per source and do not automatically retry an
unknown payment after restart. No business ledger, payment state machine or scheduled
sweep service belongs in the SDK; an explicitly requested spend-all transfer follows
the same chain-owned send pipeline.

## Runtime

The application owns cancellation, task admission, readiness and shutdown.
`Supervisor` drives networks independently: bounded jittered retries for transient
failures; quarantine for terminal failures or panics. An SDK caller can resume an
in-memory quarantine; the server clears one through an online rescan of that network
or a restart. A confirmed reorg needs a rescan, online or offline.

Readiness is `all(Ready | Quarantined) && any(Ready)`. All-quarantined is unready
without exiting. Historical coverage remains separately visible. Health reads
memory rather than making RPC calls; transport counters are read from each network's
current nodes, without adding an HTTP dependency to wallet/indexing contracts.

Composition is static: the server reads its configuration once, composes every
chain once over one repository, runs one `Supervisor` over the composed index and
serves one handler state built at startup. Adding or removing chains, assets or networks is a
configuration edit and a restart. Two maintenance actions run live, as routes over
SDK operations and model stores composed in `main.rs`:

- RPC nodes. A network's `rpc.endpoints` in the file only seed the server's
  `rpc_endpoints` table (model `RpcEndpoints`) on first start; once a network has a
  row, the row is its node list on every start (`sdk_config::Chains::with_endpoints`).
  `Chains::compose_available` composes each section's chain over a `json_rpc::Swap`.
  An edit is checked by `sdk_config::ChainNodes::verify`, the chain's own
  `Chain::verify` over the candidate (a closure the composition root hands the
  routes), stored, then swapped in; calls in flight finish on the node they reached,
  and a failed check changes nothing.
- Rescan of one network. `wallets::Wallets::pause` refuses new wallet registrations
  and sends on that network and waits for those running, then
  `indexing::Control::rescan` pauses, purges (`IndexStore::purge`), resets
  the indexer, discards progress and resumes. Other networks keep running; nothing is
  persisted between steps, because a restart after the purge simply scans from the
  birthdays.

The server's SQLite file is one `sqlx` pool, opened by
`indexing::sqlite::Database::open_with(path, models::SCHEMA)`: an empty file gets
indexing's schema and the server's own table (`rpc_endpoints`,
`apps/server/src/models/schema.sql`) in one transaction and is stamped with
`PRAGMA application_id` and a `user_version` fingerprinting both texts. There are no
migrations: a file with any other identity is refused with "delete the database file".

After-commit listeners and health notifications cannot veto writes or promise reliable
external delivery. Applications needing transactional reactions implement them inside
their repository; reliable external delivery is their responsibility.

Shutdown stops HTTP admission and drains handlers/submissions while indexing can
still reconcile ambiguous transactions. Then stop and join indexing and storage work.
Blocking database work runs off async executors with bounded admission. No listener,
HTTP readiness rule or application custody policy belongs in a bounded sync pass.
