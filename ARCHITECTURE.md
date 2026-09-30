# Architecture

## Crate map

Every crate's README is its rustdoc (`cargo doc --workspace --no-deps --open`): why it exists,
its concepts, its API map, a compiling usage example, and its neighbours. Layers are the
`rust/layers` rules in `linter.toml`, which list the layers each one may depend on (see
[Dependency direction](#dependency-direction)); nothing depends on `apps` or `examples`.

| Crate | Path | Purpose | Layer | README |
|---|---|---|---|---|
| `crypto` | `packages/crypto` | Secret key material, secp256k1 and Ed25519 keys, ECDSA/Schnorr/Ed25519 signing, the signing request/result vocabulary, and the in-process key pair every chain signs through | packages | [README](packages/crypto/README.md) |
| `decimal` | `packages/decimal` | Exact base-10 amounts: canonical positional text, numeric ordering, atomic-unit conversion | packages | [README](packages/decimal/README.md) |
| `json-rpc` | `packages/json-rpc` | Bounded JSON-RPC transport: one URL per node; retry and failover are decorators | packages | [README](packages/json-rpc/README.md) |
| `http-server` | `packages/http-server` | Axum server hardening: bearer auth, transport security, limits; the two health route paths (`payment-server` serves them) | packages | [README](packages/http-server/README.md) |
| `sdk-config` | `sdk/config` | The configuration document and the registry that hands each named section to the chain that registered for it; `Rpc`, `Repetition`, `Cadence` as plain data; TOML behind feature `toml` (off by default). It names no chain crate | config | [README](sdk/config/README.md) |
| `base` | `sdk/base` | Chain-neutral values and capabilities: address, network, asset, amount, block, sign, broadcast | base | [README](sdk/base/README.md) |
| `indexing` | `sdk/indexing` | Append-only, chain-neutral sync into history, checkpoint, and outputs, with the blocks above each chain's confirmed line held in memory; the store and chain contracts, the SQLite store that implements them (feature `sqlite`), and the supervisor that drives them (feature `supervisor`) | indexing | [README](sdk/indexing/README.md) |
| `wallets` | `sdk/wallets` | Wallet factory contract and the `Wallets` registry: sends, batches, and the asset display metadata balance and history are scaled by | wallets | [README](sdk/wallets/README.md) |
| `chain-bitcoin` | `sdk/chains/bitcoin` | Bitcoin: RPC, blocks, UTXO wallets, send pipeline | chains | [README](sdk/chains/bitcoin/README.md) |
| `chain-ethereum` | `sdk/chains/ethereum` | Ethereum: RPC, blocks, ETH and ERC-20 wallets, send pipeline | chains | [README](sdk/chains/ethereum/README.md) |
| `chain-solana` | `sdk/chains/solana` | Solana: RPC, finalized blocks, SOL wallets, send pipeline | chains | [README](sdk/chains/solana/README.md) |
| `payment-server` | `apps/server` | The one application, a binary with no library target: config, storage, chain composition, supervision, and the HTTP routes, wire models, and OpenAPI contract it serves (`src/api/`, over `wallets` and `indexing`; no chain, no storage) | server | [README](apps/server/README.md) |
| `payment-examples` | `examples` | Runnable scenarios: one directory per scenario, every chain inside it as setup | examples | [README](examples/README.md) |

How they fit: a chain crate implements the `indexing` contracts (`BlockSource`,
`BlockInterpreter`) and the `wallets` contract (`WalletFactory`); an `indexing-*` store persists
what the chain's `Indexer` interprets; the repository answers balances and
history, each completed by the blocks its index holds above the confirmed line;
`payment-server` (or an example) composes one stack per chain and then uses only
`wallets::Wallets`, `indexing::Indexing` and the query handles beside it.

## One process and one composition root

`apps/server` is the only application crate, the only executable and the
composition root: a distributable binary that loads its configuration and
serves the HTTP API, with no library target. Its routes are the module
`src/api/`, which may not name a chain crate, `sdk_config` or a storage
backend. Inside `apps/server`, only `src/chains.rs` names a chain crate. Both
are held by `scripts/check-chains.sh` check 8, because `linter.toml` layers are
Cargo packages and cannot see inside one.

Each chain composes itself: its `Config<A>`, `connect` and `compose` live in
its own crate's `src/config.rs` (`docs/CHAIN_LAYOUT.md` section 4), so a
library embedder builds a chain from a value, with no file and no parser.
`sdk/config` is the other way in: a chain registers itself under a name
(`impl sdk_config::Chain` beside its own `Config`), and a document's section
of that name is parsed by that chain and nobody else. Adding a chain is one
`registry.register::<chain_X::Config<AssetTag>>("x")` line in
`apps/server/src/chains.rs`.

Holding several of them is `wallets::Composition`, in the SDK rather than in
this binary: every verified node is a `wallets::Connection`, and
`Composition::verify_then_build` opens storage only after every identity check
has held, composes each chain over that store, and returns the three things a
process runs on — one `indexing::Index`, one `wallets::Wallets<A>`, and one
`indexing::Schedules`. An embedder that already holds its verified chains calls
`Composition::build` instead.

`main.rs` reads and validates one configuration document — TOML for a `.toml`
path, JSON otherwise — turns its `chains` table into `sdk_config::Chains`,
prints whatever each chain asked an operator to be told, and calls
`Chains::compose`, which is `verify_then_build` with the document in front of
it. `main` then imports the configured startup wallets, starts synchronization and submission supervision, builds the
router over `State { Wallets, Index, history, balance, readiness }`, and supervises shutdown —
without naming a chain, a storage backend, or an asset.
`examples/all_chains/main.rs` is the same composition with the chains stated
in code instead of read from a document, and without supervision — which is
the proof that a file is sugar and never the only door.

Storage is one embedded SQLite file: a single node, no external service. The
file is opened once, and `apps/server/src/storage.rs` builds one
`indexing::sqlite::Repository` over it that serves every configured network:
each operation names the network it is about, so isolation is the row key
rather than a bound handle. That one value implements the two ports an index
requires — `indexing::WatchStore` and `indexing::IndexStore`, together
`indexing::Repository` — plus every consumer capability this server's chains
read (`BalanceStore`, `OutputStore`, `Purging`) and its own history query
(`ConfirmedHistory`). There
is no backend switch and no seam for one: `docs/SYSTEM_REQUIREMENTS.md` puts
HA and multi-writer deployments out of scope, which is the one thing a shared database
would add, so the configuration names a file rather than choosing an engine.

Construction is intentionally visible. There is no process facade, app-local
service facade, separate wallet process, separate indexer process, or internal
wallet/indexer HTTP protocol.

```text
main (apps/server)
  |- Storage: one SQLite file
  |    `- one Repository, every network ---+-> Bitcoin Indexer ---+
  |       (WatchStore + IndexStore, plus   |-> Ethereum Indexer -+-> Index
  |        the reads this server's chains   `-> Solana Indexer ----/
  |        need; also the durable mirror of each Indexer's watch list)
  |
  |- "btc"  -> Bitcoin factory/sender ---\
  |- "eth"  -> Ethereum factory/sender ---+-> Wallets<AssetTag>
  |- "usdc" -> Ethereum token factory ---/     (factories only: it makes a
  `- "sol"  -> Solana factory/sender ----/      wallet and keeps nothing)

sync task: Supervisor -> Index                  (each unit reads its own watch list)
HTTP (src/api):  State { Wallets, Signers, Arc<Index>, History, Balances, readiness }
                 Signers is the id -> wallet map: what this process can sign
```

Each configured chain has one long-lived JSON-RPC client shared by its indexing
source and wallet-side capabilities. Retry and ordered endpoint failover are
configured once for reads and deterministic preflight; the architecture does
not invent a universal chain RPC interface. Ethereum raw submission is one
attempt against the first configured endpoint, so failover cannot hide whether
an earlier endpoint accepted the envelope. That is stated by the chain, per
call, as `json_rpc::Request::once` (`Repetition::Forbidden`), never by
deployment configuration: a URL typo must not be able to double-spend a nonce.
The chain's send pipeline (its `Outbox`) owns any exact-byte replay and
reconciliation.

RPC transport configuration is one `json_rpc::Endpoint` per node: the
provider's URL verbatim — path and query included, because an API key often
lives there — beside the headers and the bounds that node runs under, with
`${NAME}` references for secrets that only the composition root resolves
(`json_rpc::Secrets`). Two failover nodes therefore carry two different
credentials, and the transport claims no URL parameter of its own. Repetition and the
endpoint list are properties of the deployment, not of a node, so they stay
beside the list: `Retry::new(Failover::new(vec![Http, Http])?)`, where one
attempt is one complete round over the list. Composition builds the transport
and native RPC client explicitly; chain crates never construct HTTP clients.
`RpcClient::connect(transport, &network)` is the only way to obtain a client and
verifies chain identity before returning (Bitcoin: network and node readiness;
Ethereum: `eth_chainId`; Solana: the configured genesis hash and the finalized,
executable Memo-v3 program). Ethereum additionally proves the node can trace a
block (`RpcClient::validate_tracing`, below). All checks happen before storage
opens. Every
chain then builds its indexer with the synchronous, I/O-free
`indexer(IndexerParams)` over that verified client, and
`WalletFactory::new(config, &rpc[, index reads])` shares the same client and
takes only the index reads its wallets use (Ethereum none, Bitcoin the
`OutputStore` view, Solana its indexer for recovery). The transport defaults to
three endpoint rounds with 250 ms initial backoff capped at 2 seconds; a
deployment may set `repeat = 'off'`, and a `Repetition::Forbidden` call is one
execution against one endpoint regardless.

`apps/server` chooses its own `api::AssetTag` (`[a-z0-9][a-z0-9-]{0,31}`) for the
generic asset-key parameter of `Wallets`; the configuration names the tag of
each factory and the wire contract carries it as an open string validated
against the registered tags. Native ETH and an allowlisted ERC-20 such as USDC are
separate registrations backed by separate `WalletFactory` configurations,
while sharing the same Ethereum RPC and nonce-coordination objects
(`WalletFactory::sibling`). One generated wallet
therefore has one fixed payment asset. This application choice does not add
asset-selection state to `sdk/wallets` or `sdk/base`.

An asset is a fact within canonical history, not a persistence boundary. Native
and token wallets on one chain share that chain/network repository handle;
they never receive separate schemas, pools, or repositories. A history page is
one repository query by address and asset (`indexing::HistoryQuery`), which
`wallets::History::history` builds from a `Holder` and a `Page`, answered by
`wallets::PendingJoin` over that repository and a sample of the blocks its index
holds above the confirmed line (`indexing::Pending`).

Ethereum requires a node that traces blocks, and this is a composition concern
too. Contract-initiated ether is named by no transaction field and no log, so
`debug_traceBlockByHash` with the `prestateTracer` in `diffMode` is fetched once
per non-empty block beside the block and its receipts, and
`RpcClient::validate_tracing` traces the node's own head at startup. There is no
degraded mode and no opt-out: a node that cannot answer refuses the Ethereum
section, which under per-chain isolation leaves Bitcoin and Solana composing and
indexing normally. The traces stay in the in-memory `RawBlock` for one
interpretation pass; nothing persists them.

A balance is the repository's fold over the movements it stores, never a second
number read from a node, so the question "how much of the history behind it can
the repository prove it holds" is answered separately by
`indexing::Completeness`, by the repository, from the snapshot the fold was read
in. `Incomplete` is the honest default on all three chains: an address watched
from a start position later than its first funding folds low and nothing in the
rows says so. One piece of independent evidence exists, and only on Ethereum:
`chain_ethereum::BalanceWitness` asks one node what it computes for the same
holding at the *same block the fold settled at*, pinned by block hash with
`requireCanonical`, and reports whether the two agree as an `Attestation`. It is
not a value of `Completeness` and it is written onto no balance: no repository
can make or persist it, so what an agreement is worth is its caller's to decide.
It never changes the amount and it cannot fail the read; disagreement is
reported rather than resolved. Bitcoin Core has no address index and Solana cannot read an account at
a past slot, so neither chain has an oracle to pin. Two balance causes remain
open, both Ethereum's: consensus-layer withdrawals are parsed from the block
body (`RawBlock::withdrawals`) but have no home in `InterpretedBlock`, which
carries transaction drafts and output changes only, and an ERC-20 `eth_getLogs`
backfill waits on the general backfill (`docs/design/030` steps 12 and 13).

Token admission is a composition concern. The application validates the
allowlisted contract on one canonical block through the verified
`RpcClient::validate_token` before building wallet factories. Until endpoint identities are validated
separately, token-enabled composition admits exactly one Ethereum RPC endpoint.

The concrete `Arc<Index>` is cloned into the sync task and into `Wallets`,
which reads it as `Checkpoint`. Bitcoin's composition joins its repository's
`OutputStore` to its index's pending blocks (`wallets::PendingJoin`), which its
`WalletFactory` spends; `Index` does not force UTXO semantics onto account
chains, and routes no query at all. These are views of already-composed state, not parallel
registries.

## Dependency direction

```text
apps/server
  -> sdk/chains/{bitcoin,ethereum,solana}    (src/chains.rs only)
  -> sdk/config                              (with feature "toml")
  -> sdk/wallets
  -> sdk/indexing
  -> packages/{http-server,json-rpc}

apps/server/src/api                          (a module, held by check-chains.sh check 8)
  -> sdk/{base,indexing,wallets}
  -> packages/http-server

examples -> the same crates as apps/server

sdk/chains/{bitcoin,ethereum,solana}
  -> sdk/base
  -> sdk/config
  -> sdk/wallets
  -> sdk/indexing
  -> packages/{crypto,json-rpc}

sdk/config -> sdk/{base,indexing,wallets}, packages/json-rpc
sdk/wallets -> sdk/base, sdk/indexing, packages/crypto
sdk/indexing -> sdk/base (+ rusqlite and tokio under its default features)
sdk/base -> packages/{crypto,decimal}
```

- `sdk/chains/*` holds concrete chain implementations only; chain-neutral crates
  live at `sdk/<capability>` (`sdk/base`, `sdk/config`, `sdk/indexing`,
  `sdk/wallets`).
- `sdk/config` may not depend on a chain crate. Registration is the chain
  handing itself in, which is the only reason the dependency runs
  chain → config and never back.
- A package may depend on external crates or another package, never SDK/apps.
- `sdk/base` imports no concrete chain, RPC, indexing, or wallets.
- `sdk/indexing` imports no chain-native block, backend record, wallet, or HTTP
  type.
- Concrete chain crates implement generic wallet/indexing contracts while
  retaining their native protocol semantics.
- `sdk/indexing` is one crate in two parts: `storage/` is the store ports and
  the two implementations of them, `sqlite` on a file and `memory` on maps --
  the one abstraction with a second implementation -- and the root is everything else, the
  vocabulary,
  the contracts, the pass and the supervisor that drives it. The routes in
  `apps/server/src/api` use the public root only and name neither store.
- Inside `sdk/indexing/src/storage/` three rules hold, and each is checked:
  1. A store may import the shared storage files and the engine. Nothing
     imports a store: not the engine, not a shared storage file, not another
     store. Declaring the two modules in `storage.rs` and re-exporting them
     from `lib.rs` as `indexing::sqlite` and `indexing::memory` is not
     importing them, and test-only code may use both
     (`scripts/check-chains.sh`, check 12).
  2. Every directory directly under `storage/` is one store. Shared code is
     files, never a folder (`layout` in `linter.toml`).
  3. Every file directly in `storage/` is shared by every store. A store's
     root module is inside its folder (`sqlite/mod.rs`), not beside it
     (`sqlite.rs`) (`layout` in `linter.toml`).
- Nothing depends on `apps/server`.

## Ownership

### Generic packages

`packages/*` must remain useful outside a blockchain project:

- `crypto` owns secret memory, generic cryptographic operations, the
  vocabulary of one signing job (`SignRequest`, `SignedPayload`,
  `SignerError`), and `KeyPair`, the in-process key that answers one on either
  supported curve — every chain's key wraps one, so no chain owns signing
  code; the asynchronous `Signer` capability over that key belongs to `base`,
  because a remote signer is an SDK concern and a package owns no async;
- `decimal` owns one exact base-10 value, its canonical text, and atomic-unit
  conversion; it knows nothing about assets, chains, or currencies, and
  `base` re-exports it as `Decimal`/`DecimalError`
  ([ADR-0029](docs/adr/0029-wrap-a-proven-decimal-core.md));
- `http-server` owns axum server hardening (authentication, transport
  security, request limits) and the two health route *paths*
  (`LIVENESS_PATH`, `READINESS_PATH`); `apps/server/src/api/health.rs` serves and
  documents them, because readiness is an application fact;
- `json-rpc` wraps `jsonrpsee` with a bounded one-endpoint transport plus
  `Retry` and `Failover` decorators over the same two-method `Client` trait.

There is no storage package. The six store ports and the one file that answers
them are `sdk/indexing/src/storage` and `sdk/indexing/src/storage/sqlite`: a
port written against this project's block, history and checkpoint vocabulary is
not useful outside a blockchain project, which is what `packages/*` is for.

Repository architecture rules are enforced by the external `linter` against
`linter.toml`, plus `scripts/check-chains.sh`; neither is a workspace crate.

Packages contain no wallet, chain, indexing, asset, or transaction policy.
`json-rpc` redacts endpoint text and header values in `Debug`, and its errors
name a parameter but never a value, a host or a URL. Solana is configured with
exactly one endpoint and every Solana call is `Request::once`; its indexing loop
and outbox, not generic transport, own every explicit retry.

### Configuration

`sdk/config` owns the **document** — reading it (JSON always, TOML behind
feature `toml`, off by default, so no library crate links a format; CI proves
it with `cargo tree`,
[`docs/design/030 §8 (ex-028)`](docs/design/030 §8 (ex-028).md) section 6.2),
splitting it into named sections, and reporting *which section* failed and
why. It also owns the shell every chain's section shares: the endpoint list
and its repetition (`Rpc`, `Repetition`), the cadence a chain indexes at
(`Cadence` — a `Duration` and an `indexing::SyncPolicy`, both the type their
consumer takes), and the composed `Transport` they produce.

It owns **no chain's meaning**. Each chain states what its own section means
by implementing `sdk_config::Chain` for its own `Config`, and `Registry`
stores nothing but a name and a closure that deserializes it — which is why
this crate depends on no chain crate and why adding a chain edits no crate but
that chain's and one line of the composition root.

The types are the API: public fields, a `new` that fills the defaults `serde`
fills, a `validate` on the value rather than inside a `Deserialize`, and no
`Serialize` on anything that carries a resolved credential. A chain is
constructible in plain Rust with no document anywhere, which is what
`examples/` runs on.

### Chain base

`sdk/base` contains the explicitly approved values and capabilities
that genuinely apply across substantially different chains:
network/chain/asset metadata, persisted chain-neutral identities
(`ChainName`, `Network`, `AssetId`, `Address`, `TransactionRef`), exact
decimals, block identity, key/signature values re-exported from `crypto`,
`Signer` (the one signing item it defines itself), transaction snapshots, and
broadcast results.

It is not a home for UTXOs, Ethereum envelopes, chain RPC DTOs, wallet
construction, indexing policy, or a universal transaction representation.

### Concrete chains

A concrete chain owns everything that disappears when that chain is deleted:

- canonical address parsing and network validation;
- native RPC methods and wire DTOs;
- native block and transaction parsing;
- UTXO/script/fee or account/nonce/gas rules;
- transaction building, signing, encoding, and broadcasting;
- wallet factory, balance implementation, and batch sender; and
- block source and interpreter.

Every chain crate follows one layout, defined with Ethereum as the worked
example in `docs/CHAIN_LAYOUT.md` and enforced by `linter.toml` plus
`scripts/check-chains.sh`: `src/native/` holds all protocol code and never names
`indexing` or `wallets`; every other `src/` file is the adapter for exactly one
SDK contract (`config.rs`, `source.rs`, `interpreter.rs`, `indexer.rs`,
`wallet.rs`, `builder.rs`, `sender.rs`, `key.rs`, `identity.rs`,
`error.rs`). History is not
a chain contract: a factory states only which asset its wallets hold
(`wallets::FactoryProfile::asset`), and the chain's `Config` declares what an
amount of each of its assets is (`base::Units`, folded by
`wallets::Composition`).
The chain vocabulary is fixed: `RawBlock` → `parse()` → `Block` → `Transaction`
→ `movements()` → `Movement`. All three chains follow it. Every chain sends
through one `Outbox` in `sender/` with the same files and stages (reserve →
prepare → sign → submit → settle; `docs/CHAIN_LAYOUT.md` section 7); only the
reserved resource differs: Ethereum senders and nonces, Bitcoin outputs,
Solana senders.

The `chain-solana` package owns canonical `SOL` metadata, its native-only
`AssetKind`, the `Network` (slug plus genesis hash), Solana addresses, Ed25519
key handling, lamports, RPC DTOs, legacy transfer-plus-Memo transactions,
account policy, source/interpreter translation, and the factory's send
pipeline (`sender/`: the `Outbox` with sender reservations, dispatch, and
recovery of unknown outcomes, plus its one-method submission-task
registrar). Its layer depends only on packages, base, indexing, and wallets.
No Solana message, account, slot, envelope, or outbox type becomes a generic
protocol abstraction.

Its protocol stack is the exact modular Anza/SPL dependency family selected in
ADR-0027, not `solana-client`, a monolithic SDK, handwritten wire encoding, or
copied System discriminants. The workspace baseline is Rust 1.91; the reverted
Rust 1.85 experiment is historical evidence, not an active requirement. An
exact scratch-only Rust 1.91 proof combined the current workspace graph with
the selected modular Solana family while retaining `alloy-consensus`,
`alloy-eips`, and `alloy-rpc-types-eth` 1.8.3; `alloy-primitives` and
`alloy-sol-types` 1.6.1; and rusqlite 0.40.2. Repository manifest work must preserve
that graph and repeat the locked Rust 1.91 checks and focused regressions;
divergence stops the affected step.

Deleting one chain must leave the other chain and every generic crate coherent.

### Indexing

`sdk/indexing` owns:

- exact `(chain, network)` identities and complete block-reference checkpoints;
- address filters with native-position birthdays;
- canonical transaction, movement, and live-output facts;
- block-source/interpreter contracts;
- the two persistence ports an index requires — `WatchStore` and `IndexStore`,
  together `Repository` — the consumer capabilities a store may offer beside
  them (`BalanceStore`, `OutputStore`, `Purging`), and the history query a
  repository may expose of its own (`ConfirmedHistory`);
- the confirmed line each chain states about itself, the pending blocks held in
  memory above it, and the sample of them a reader narrows by a store's answer;
- single-network synchronization, including the half of a pass that fills in
  blocks *at or below* the checkpoint for an observation watched from before it
  was registered, through the same commit; and
- the multi-network `Index`.

`Indexer` is one network's whole indexer: it owns that network's watches, the
blocks it holds above the confirmed line, its chain adapter and its reference to
the store, and its bounded pass is a procedure over those fields rather than a
call into a second object holding a second copy of them (`docs/design/030 §8 (ex-032)`). The
pass therefore takes no watch set as an argument and shares no pending block
with a reader. Scheduling, retry delays, quarantine and shutdown are not in it at all:
they belong to the optional `Supervisor`, which calls `Indexing::sync` and owns
no second implementation of scanning or committing — which is why the crate
builds, and a caller drives bounded passes by hand, with the `supervisor`
feature off.

One network's `Indexer` implements `indexing::Indexing` — observing, advancing, and saying where it got to, all of
which a `Repository` alone answers, and that is the whole of the indexer
surface: no query is on it, so a store folding nothing is composed and
supervised exactly like one that folds everything. Neither read the routes
serve is taken through it. What an index exposes to a reader is its confirmed
line (`Checkpoint`) and a sample of the blocks it holds above it
(`indexing::Pending`); `wallets::PendingJoin` joins one repository's
`ConfirmedHistory` pages, its balance fold and its live outputs to that sample —
sampled before the repository is read and narrowed by the repository's own
answer (`indexing::Checkpointed`), never by a checkpoint read beside it, so a
commit landing inside a read is counted once. Each chain builds one for its own network, and `wallets::Composition`
collects them into the single `Arc<dyn wallets::History>` and
`Arc<dyn wallets::Balances>` the routes declare.
`Index<C: ?Sized = dyn Indexing>` and `Supervisor<C: ?Sized = dyn Indexing>`
are generic over the child trait with that default. There used to be a wider
trait above it, `Indexing + Balances`, carried by every composed child and read
by none of them — the courier of `docs/DEFECT_PATTERNS.md` section 1.1 — and
removing it is what left one trait here. `Indexer` names the type and nothing
else.

The index owns its durable selection: `Watching::watch` writes the `WatchStore`
row and each `Indexer` keeps the hot set its passes read, so nothing calls back
into the caller to ask (`docs/design/030 §8 (ex-029)`). Wallet SDK and
embedding-application code own identities, secrets, asset selection, and
birthdays. A pass never queries custody state directly, and no watch row carries
key material.

`BlockPosition` is the native monotonic RPC coordinate: Bitcoin height,
Ethereum block number, or Solana slot. It drives traversal, canonical lookup,
restart, readiness, and birthdays. `BlockHeight` is the produced-block count
and drives confirmation arithmetic and history and output ordering. A
persisted `BlockRef` carries both coordinates,
its hash, and one atomic optional parent value pairing parent position with
parent hash. Only genesis has no parent.

`IndexStore::commit` is the **one** write: one `IndexedBatch` carrying the blocks,
the per-observation `Progress` they prove and the checkpoint they were planned
under, committed atomically. Whether the line moves is derived from the blocks
(`IndexedBatch::checkpoint`), so a batch of blocks the network already committed —
re-inspected for an observation that was not watched at the time — is the same
call with the affected observation simply further behind. The store is
append-only for reorg purposes: every block it receives is at or below the chain's
confirmed line, so no row it holds is ever undone and there is no removal and no
rollback data (`docs/design/030 §8 (ex-025)`).
`ConfirmedHistory` and `OutputStore` are read projections over this lifecycle.
A page re-reads the checkpoint after its rows and reports a `Conflict` if it
moved, so no page mixes two canonical views.

`indexing::sqlite` is the store: one file holds every network's checkpoint,
history, movements, outputs, per-observation progress, and the watch list, every table
framed by `(chain, network)`, and every write is one `BEGIN IMMEDIATE`
transaction run off the executor, one whole operation at a time. It is the only
store the server composes. `indexing::memory` (feature `memory`) is the second
shipped store and the second implementation the shared contract suites are held
against, so `WatchStore`, `IndexStore`, `ConfirmedHistory` and `OutputStore` stay
statements about the contract rather than about SQLite; one
`memory::Repository` holds every network exactly as one file does, and is for a
process that wants nothing on disk rather than for tests alone. `sdk/indexing/tests/minimum_contract.rs` is the other proof: a
non-`Clone` sink implementing the two required ports and no read method at all
observes, commits and restarts, reaching the index as `Arc<Sink>` through the
forwarding impl each port carries. Stored rows never appear in a chain
interpreter or a generic consumer.

The `watch` table is the durable watch list, behind `WatchStore`. What it
observes is application state: a purge of one network leaves those rows in
place, which is what lets that network re-index from the earliest birthday it
records. What each row *claims* is not spared — the recorded progress, the
running credit and debit totals, and the balance basis are all statements about
rows the purge has just removed — so every watch of the purged network comes back
with nothing proved.

The durable set a pass writes is deliberately limited to checkpoint,
address-primary canonical history, and live outputs. The blocks above the
confirmed line are held interpreted in memory on the `Indexer`, never written:
they are the blocks the pass already fetched and already interpreted, kept
instead of thrown away, and a reorg shallower than the line is a truncation of
that list with no store I/O at all. Confirmation, readiness, status, watches,
revisions, raw blocks, and event feeds are not synchronization persistence. Process-local submission
leases, exact outgoing envelopes, request identities, and reconciliation state
are also not indexing-owned records.

A watch row carries no key material: it names an address, the asset its history
is read in, a birthday and the caller's id. Custody policy belongs to the
operator and the calling application (§Wallet collection). One row is keyed by
`(chain, network, asset, address)` and its identity is unique across the whole
file, so neither an observation nor a name can be silently replaced — which is
also why one address is observed once per asset and no oftener.

### Wallets

`sdk/wallets::Wallets<A>` unifies factories and stores nothing else:

- one `WalletFactory` per asset key `A`, with the network, asset identity, and
  `Authorship` its `FactoryProfile` declares (`Wallets::add`, the one
  registration API). A wallet carries no display metadata: what an amount of an
  asset is belongs to the asset, declared by its chain's `Config` as
  `base::Units` and resolved by `AssetId` at the edge;
- `create`/`from`, which make a wallet, hand it back, and keep no copy;
- `accepts`, `parse`, `secret`, `asset`/`assets`, `author`, and `send_all`.

**Spending is a declared capability, not every wallet's.** `wallets::Wallet` is
`indexing::Holder` + `base::Signer` — an address, an asset, and a key — and
that is what `create`/`from` return. `SingleSender` and `TransactionFactory`
belong to `wallets::Author`, which only `Wallets::author(asset, wallet)` hands
out, and only when that asset's factory declared an `Authorship`
(`FactoryProfile::authorship`, which is also the batch `Sender` `send_all`
uses). An asset this system receives and reports but never spends — a Solana
SPL holding — declares `None`, so both refuse at that one lookup and a holder
of `Arc<dyn Wallet>` cannot even write the call: the receive-only case is a
fact of the type system rather than four gates inside a chain.

Three other owners answer what it stopped answering (`docs/design/030 §8 (ex-029)`):
`indexing` owns the durable list of what is observed (`Watching::watch`), the
chain owns building and signing, and the application owns the id -> wallet map
and where keys live. In `payment-server` that map is `api::Signers`,
two methods over a `BTreeMap`, deliberately not a type in `wallets`: a
container there called anything at all would be `Wallets` renamed, which is the
objection.

`WalletFactory` constructs a concrete wallet by generating or importing secret
material without returning that secret and validates an existing wallet against
its network, native configuration, and shared operational dependencies. Native
type inspection stays private to the factory; application code uses neutral
wallet capabilities. Registration owns the factory exactly once.

Bitcoin's local `KeyPair` owns only its secret and signing; it names no network.
Its factory attaches it with the `WalletConfig`'s network and address type,
derives the address, and adds UTXO, fee, and submission capabilities to
construct a full wallet. Factory generation and import use the same local derivation.
The local key alone does not participate in indexing.
Ethereum's local `KeyPair` similarly owns address derivation, formatting, and
signing; EOA derivation needs no network or asset configuration. Its factory
attaches chain/asset policy, accounts, and transaction coordination.
Solana's local `KeyPair` names its address, the Ed25519 public key itself, and
names no cluster. All three wrap the same `crypto::KeyPair`: Bitcoin and
Ethereum import a secp256k1 scalar, Solana an Ed25519 seed, and each chain's
`key.rs` is one delegation to it.

Membership is proved rather than recorded: `Wallets::accepts`, `author` and
`send_all` put every wallet through `WalletFactory::validate` (`Origin`), which checks the
same type, the same configuration and the same shared RPC and outbox handles.
That is the property a batch actually needs, and it is stronger than a tag
written down when a wallet was filed somewhere.

A watched address is not a kind of wallet: it is the absence of a wallet beside
an index watch row. `index.watch(id, &dyn Holder, Birthday)` takes a wallet or a
bare `indexing::Subject { address, asset }`; both are indexed and both serve
history, and "watch-only" is the derived fact that this process holds no key
for a watched id.

**The order between the two durable facts about one id is the contract.**
`index.watch` commits its row *before* the caller files the key. A crash between
them leaves a watched address with no key — exactly a watch-only address, which
is visible in `index.watched`, whose history is correct, and which re-importing
the key repairs. The other order would leave a key nothing observes: a wallet
reporting zero balance and empty history forever, with nothing saying why.

For `payment-server`, `A` is its `api::AssetTag`, the user-facing tag of one
registered factory. The map remains generic and other embedding applications may
choose a different key type.

A restart splits the same way. What is *observed* comes back from the index's
own `WatchStore` rows — each with the block through which it has been inspected,
from one snapshot — read once by each `Indexer` before its first pass, at no
cost to the caller. What can *sign* comes back only from the application's key
store: `apps/server` rebuilds it from `config.wallets[]`, each entry naming the
environment variable holding its key and the birthday to watch from. The index
holds no key material at all, and nothing reads a row that carries it: the export
subcommand went with the redb backend it was the only reader of.

### Public HTTP

`apps/server/src/api` owns public routing, Utoipa schemas, transport validation,
authentication, limits, HTTP errors, and encoding. HTTP state contains the
abstract factory map, the wallets this process holds keys for (`Signers`), the
composed `Arc<Index>` with the two query handles beside it, and readiness
state, not repositories, sources,
interpreters, or concrete chains. The crate
has no chain dependency at all, which `linter.toml`'s `api` layer enforces.

Handlers are grouped by resource. Every endpoint-specific request and response
struct is declared immediately above its one handler. A handler is limited to
extraction, one application call, error/status mapping, and encoding. Shared
wire types exist only for exact reuse; domain types remain with their domain.

## Indexing flow

```text
Supervisor -> Index::sync (no selection argument)
    -> chain Indexer, each against its own watch list
    -> source tip and source confirmed tip
    -> verify committed checkpoint against canonical hash (else ConfirmedReorg)
    -> drop held blocks the source no longer calls canonical
    -> source produced blocks after the newest known position
    -> interpreter(native block, active addresses)
    -> CanonicalBlock::new -> held above the confirmed line (facts only)
    -> IndexedBatch::new(checkpoint, [facts], claims) then IndexStore::commit,
       for every held block at or below that line; the claims are every
       caught-up observation whose next required block this one is or passes
    -> Listener::committed(&BlockCommit)   after each commit, synchronous
    -> fill in what is owed, with whatever is left of the batch budget:
         WatchStore::all       once per process: each watch and its own
                               indexed_through, from one snapshot
         source blocks from the lowest owed position up to the checkpoint
         interpreter(native block, the addresses that still owe it)
         IndexStore::commit    the same call: blocks at or below the line,
                               plus the claims they prove, one transaction
```

A `Birthday::At` below the checkpoint is honoured rather than discarded. Each
observation carries one contiguous, hash-verified `indexed_through` and resumes
after it; a pass takes the lowest `next_required` across every observation that
has not reached the line, reads one shared range up to the checkpoint, and
advances each of them to the highest block it read at or above that observation's
own resume point. **An observation that has not reached the line is not in the
forward selection** — `WatchProgress::caught_up` is that decision, with both
possible behaviours written out in its own doc comment — so a wallet imported with
an old birthday does not see a payment arriving today until its catch-up reaches
the present, and in exchange one reference describes the whole of what has been
read. Forward progress spends the batch budget first, so a network behind its tip
fills in nothing at all, and `SyncPolicy::max_backfill_blocks` refuses a start too
far down at `watch` rather than in the background. `SyncStatus::backfill` says
what is left, and the *same* `caught_up` predicate is what each repository asks
of its own watch rows, in the snapshot it answers from, before it claims
anything about a range: a balance or a history page of an address still being
inspected below the line cannot claim `CompleteHistory`, and its spendable
outputs are refused until the inspection reaches the line. `docs/INDEXING.md` has the guarantees; `docs/design/024` has the
argument.

All configured historical wallets are registered before the first sync. A
fresh network locates the first produced block at or after its earliest birthday
position and uses that block's real parent as the anchor; an empty filter set
anchors at the actual produced source tip. A generated wallet begins at the
checked successor of the current checkpoint position. If that native position
is skipped, the wallet activates at the first later produced block.

The persisted checkpoint is valid for the authoritative historical address set
that produced it. A changed set below the checkpoint requires recreating and
rescanning the network, because synchronization resumes from the checkpoint and
never revisits blocks behind it.

On restart, the application reloads the authoritative wallet/birthday set
before synchronization. A changed birthday beneath the checkpoint requires an
explicit rescan of only that exact `(chain, network)`. Such a rescan must never
drop the central database, touch another network, or delete
application-owned `payments_wallet` rows.

A reorg above the confirmed line discards the held blocks from the fork
upwards and indexes the replacement branch normally; nothing durable moves,
and no backend code runs. A reorg *below* it means the chain took back a block
already committed as irreversible: terminal `ConfirmedReorg`, which quarantines
that network and needs a rescan of it. That is impossible on Ethereum and
Solana, whose lines are consensus guarantees (the node's `finalized` block, and
a rooted slot); on Bitcoin, which states no finality, it means a reorg deeper
than the configured `confirmation_depth`, whose default is six.

### The rescan

`indexing::Purging::purge_network(network) -> PurgedRows` is the one port
method that removes anything: one network's checkpoint, history, movements,
block credits, unmatched spends, live outputs and recorded progress,
atomically, on every backend, reporting the row counts it removed. It sits
outside `Repository` deliberately — the offline rescan is not indexing, and a
third-party sink that offers none still indexes correctly. It names the one
network it purges, and neither the watch list nor another network's rows, which
is how the constraint above is enforced rather than merely stated.

`apps/server/src/rescan.rs` exposes it as
`payment-server <config.json> --rescan <chain>/<network>`, an **offline**
subcommand: it reads the configuration, opens storage, deletes, prints the
counts, and exits. No chain connects, no supervisor starts, and the listener is
never bound — purging under a live API would serve an empty history that looks
authoritative and would break checkpoint-bound history cursors mid-page. A
malformed argument and a network the file does not configure are refused before
storage is opened. The next ordinary start finds no checkpoint and re-indexes
the network from `SyncPlan::earliest_position()`.

## Transaction flow

```text
abstract wallet request
    -> concrete chain-native builder
    -> unsigned native transaction
    -> chain computes signing request
    -> injected Signer
    -> chain verifies and inserts signature
    -> exact signed bytes
    -> native broadcaster
    -> submitted transaction ID
    -> indexing observes canonical inclusion/confirmation/reorg
```

Submission is not confirmation. No broadcaster polls receipts to establish a
second confirmation system.

Bitcoin preserves outpoints, every input/output, scripts, checked satoshi
fees, per-input signers, and deterministic change. Ethereum preserves chain ID,
nonces, EIP-1559 fees, typed envelopes, recovered signer, receipts, logs, and
the configured native-or-token asset. An ERC-20 wallet exposes only that token
for balance, send, and history movements while retaining native ETH as network
fee metadata. The shared wallet surface does not replace those native models.

For batches, validate every request and the one-asset constraint before the
first external effect. Bitcoin may produce one multi-source native transaction.
Its `Outbox` selects and reserves outputs atomically, so concurrent sends never
spend the same output; a reservation lasts until the index shows the output
spent or the node refuses the transaction, and an unknown outcome is recovered
by exact-id lookup or byte-identical replay before the next send from its
address.

One Ethereum `Outbox` is shared by native and token factories. It reserves
consecutive nonces by sender address, completes whole-batch simulation,
cumulative balance checks, and signing, then broadcasts the exact envelopes in
request order. A retryable ambiguous submission retains its exact envelope and
blocks that sender until exact-hash reconciliation; a later failure reports
only the accepted prefix.

The Solana-owned `Outbox` reserves source addresses and builds one legacy
System-transfer-plus-Memo transaction per public occurrence.
It uses exactly the executable Memo-v3 program account, obtains exact fees,
checks cumulative lamports, signs, and simulates the complete batch before
ordered one-shot, endpoint-stable broadcast. An unknown result may replay only
the same bytes within the recent-blockhash lifetime and guards the source until
status or complete finalized indexed history resolves observation or absence.
Startup and the owned validator fixture require that exact Memo program to be
executable. The ADR-0027 concrete probe and supervised task wiring are
implemented; the checksum-pinned real-validator execution remains outstanding
system evidence.

Outbox state is intentionally in-process on every chain because indexing owns
no pending-transaction records and the product has no durable
outgoing-operation store. Application composition therefore requires one
active transaction writer per managed Bitcoin address, Ethereum EOA, or Solana
source. Solana
callers must not automatically retry an unknown logical payment; restart,
failover, or active-active writers can double-pay, and process-crash recovery is
not claimed.

## Runtime lifecycle

Startup order is part of correctness:

1. validate configuration and server security;
2. construct each configured chain client, including Solana's singular,
   no-retry client with redacted configuration;
3. verify every chain identity before database mutation;
4. for Solana, verify the expected Base58 genesis hash and the finalized,
   contextual executable state of the exact Memo-v3 account;
5. open the one SQLite file, validating its application id and applying every
   schema step it is behind;
6. bind one repository per configured chain/network to that file, load
   checkpoints, and initialize its filter/commit coordination;
7. construct chain indexers and `Index`; the Solana factory takes its
   `Checkpoint`, `History`, and checkpoint notification from the Solana
   indexer and runs submissions under the application's registrar
   (`WalletFactory::with_registrar`);
8. construct `Wallets` over the `Index`, register only the accepted asset
   factories, and import every
   configured seed at its stated `start` before the first sync snapshot;
9. start synchronization and the application-owned readiness/submission
   supervisors;
10. wait for every configured network to settle — `Ready` with a persisted
   checkpoint at least once, or quarantined. History still owed below a
   checkpoint does not hold this gate (`docs/design/024` section 7);
11. bind the public listener; and
12. supervise HTTP, synchronization, Ctrl-C, cancellation, submission
    reconciliation, and task joins.

`SyncStatus` still reports only pass progress (`CatchingUp` or `Ready`), and it
is still not where a failure lives. A failure is survivable now, so it has to
be remembered somewhere: `indexing::NetworkHealth` is that cache, holding one
network's `NetworkState`, the `IndexError` holding it back, its consecutive
attempt count, when the state was entered, and the `SyncStatus` of the last
pass it completed, which is where its last checkpoint is read.
`indexing::SyncHealth` owns the readiness rule over every network.

`indexing::Supervisor` drives **one task per `indexing::SyncUnit`**,
which is one per composed chain. One chain's failure is therefore only its own:

- a retryable `IndexError` retries that network forever, waiting
  `interval × 2^(attempts − 1)` capped and jittered (a `Conflict` keeps the
  healthy cadence, because it is a lost race, not an outage);
- a non-retryable one — `InvalidBlock`, `NetworkMismatch`, `InvalidRequest`,
  `ConfirmedReorg`, a terminal `Store` failure — and a panicked unit task
  quarantine that network alone, park it alive with its last checkpoint, and
  never auto-retry: refetching re-parses the same bytes;
- `SyncControl::resume(&network)` is the way out of a quarantine whose cause an
  operator fixed out of band, reachable without a restart as
  `POST /v1/status/resume` (the composition root takes the handle before
  `Supervisor::run` consumes the supervisor and hands it to the routes behind
  `api::Resume`, so the routes still name no runtime); for
  `ConfirmedReorg` it re-quarantines within one pass, correctly, and the offline
  `--rescan` above is the only remedy;
- `Supervisor::run` returns only on shutdown. Indexing no longer fails startup
  or exits the process on a chain failure, because exiting also kills the
  status surface that explains why.

Readiness is `all(Ready | Quarantined) && any(Ready)`. A quarantined chain
does not hold `/health/ready` at 503 forever — the healthy chains work, and the
dead one is reported per network on `GET /v1/status`. Every network
quarantined is not ready, and still does not exit. Startup binds the listener
once every network has settled.

The application owns one `mpsc` admission queue and `JoinSet` for Solana
submission and passes its registrar to the Solana factory. The registrar
acknowledges only after task insertion, so closure or lost acknowledgement
before insertion fails before dispatch. Account acquisition completes before
registration. Without an application registrar, `chain-solana` spawns
submission tasks on the current Tokio runtime (examples, tests); the
application never relies on that default, so no handler, wallet, or chain
object detaches a send or readiness task outside its supervision.

Graceful shutdown publishes not-ready, stops HTTP admission, serializes registrar
closure against task insertion, drains handlers, and waits for the guarded set
to become empty while indexing and historical-status reconciliation remain
available. Only then does it cancel synchronization and await storage work. An
unknown envelope has no automatic shutdown deadline. After a fatal indexer exit,
only positive historical status may clear it in-process; force-kill explicitly
accepts the documented duplicate-payment risk.

The integration environment pins `solana-test-validator v3.1.14` at commit
`3134055b562e95902233be308453fffa1c4a8902`, verifies committed SHA-256 values,
and owns ledger, ports, keys, and cleanup. Default tests use RPC doubles. The
explicit `solana_node` application target, checksum gate, resource harness,
and end-to-end scenario are implemented. It remains manual rather than CI
automation until a checked-in workflow owns the pinned fixture; no run is
claimed unless the exact artifact is locally available and verified.

Shared-schema evolution is preservation-first. Indexing migrations are
additive or backfilled under explicit validation before final constraints are
enforced. They do not introduce runtime compatibility readers, versioned DTOs,
or inferred fallbacks. A destructive per-network replacement requires explicit
operational approval and may remove only indexing-owned rows for the named
network.

## Product boundary

The architecture supports wallet generation/import, canonical
address and exact selected-asset balance, complete checkpoint-bound paginated
history for that selected asset, one or ordered batch submission, and
continuous filtered indexing. Indexing may retain unrelated canonical facts for
a watched Ethereum address; the concrete wallet projects only its configured
asset.

It does not contain deposit accounting, ledgers, payment state machines,
collection/sweep jobs, reservations, hardware-wallet workflows, remote
custody, public index-management commands, raw-block archives, event feeds, or
pre-release compatibility layers.
