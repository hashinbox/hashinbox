# Repository guide for coding agents

These rules define Payment-SDK's durable product, architecture, design, coding,
and delivery standards. Apply them to new code and improve nearby code when
safe. Preserve behavior during refactors and prove changes with observable
tests. Prefer fewer, stronger abstractions and less code.

## Source of truth

Read this file before changing code. Use project sources in this order:

1. `docs/SYSTEM_REQUIREMENTS.md` for canonical scope and acceptance criteria.
2. `ARCHITECTURE.md` for ownership and dependency direction; its crate map
   says what each crate is for in one table.
3. `docs/INDEXING.md` for indexing and persistence semantics.
4. `docs/API.md` for the public HTTP surface.
5. Each crate's `README.md` (its rustdoc) for that crate's purpose, concepts,
   API map, and usage.
6. `docs/CHAIN_LAYOUT.md` for the layout every chain crate follows and the
   only places a chain may differ.
7. Current source and tests for implemented behavior; source is authoritative
   for exact signatures.

`docs/adr/` is **not** in that order. An ADR records one decision at the moment
it was taken; its paths and type names are historical and most of them predate
the chain-unification refactor. Never rewrite an ADR to match a rename — write
a new, superseding ADR when a decision changes, and correct an existing one
only when it misstates its own outcome. `docs/adr/readme.md` states this
policy for readers.

Follow the higher-authority source when documents disagree and update the
stale document. The project is pre-release: replace unpublished internal types
and wire formats directly. Do not add compatibility aliases, legacy runtime
readers, or project-defined `V1`/`V2` DTOs. **The index file is not an
exception.** It used to be one: the redb layout was frozen byte for byte, two
committed fixtures pinned it, and `payment-server --export-keys` existed as the
one-release read-only path off a dead key tag. `docs/design/030 §8 (ex-031)` replaced that
store with one SQLite file and `docs/SYSTEM_REQUIREMENTS.md` revoked the
exception outright, so the layout, the fixtures and the export are all gone.
What a schema change needs now is what SQL has always offered: one more step in
`storage/sqlite/schema.rs`, applied in order inside the transaction that bumps
`PRAGMA user_version`, and a file this build cannot account for refused rather
than interpreted. A single-network rescan must still preserve unrelated networks
and the address registry unless their owner separately approves a change.
External Bitcoin, Ethereum, Solana, JSON-RPC, and HTTP standards remain
compatibility contracts.

`old/` and `reference/` are excluded research, not production dependencies or
architecture templates. Do not edit them unless explicitly asked.

If `.codegraph/` exists, use `codegraph explore "<symbols or question>"` before
grep/find or broad reading when locating or understanding code. If it does not
exist, skip CodeGraph; repository indexing is the user's decision.

## Product boundary

Payment-SDK's accepted target is one API process that initializes Bitcoin,
Ethereum, and Solana RPC clients and embedded synchronizers; generates or
imports wallets without returning secrets; returns canonical addresses, exact
balances, and complete history; submits one transfer or a non-empty ordered
batch; indexes caller-selected addresses; and survives restarts and reorgs.
Current source initializes all three.

Business and HTTP code must not import a concrete chain. Concrete chains are
selected once at composition.

There is currently no deposit accounting, ledger, payment state machine,
collection/sweep job, reservation system, hardware-wallet workflow, remote
custody, separate wallet/indexer service, raw-block archive, or public indexing
command surface. Do not introduce these concepts without an approved product
change or claim development in-process custody is production custody.

## Architecture and ownership

`apps/server` is the only executable and composition root; `apps/api` holds the
routes it serves and may not depend on a chain or storage crate. No crate
depends on `apps/server`.

| Path | Owner |
|---|---|
| `apps/api` | Public HTTP/OpenAPI routes and wire models over `wallets::Wallets`, `indexing::Index` and the narrow `indexing::{History, Balances}` handles beside it, plus `Signers`: the id -> wallet map of what this process can sign |
| `apps/server` | Config, storage choice, concrete chain composition (`src/chains{.rs,/}` alone), task supervision, shutdown |
| `sdk/base` | Small approved chain-neutral values, signing requests, transaction snapshots, broadcast capabilities |
| `sdk/chains/bitcoin` | Bitcoin addresses, RPC, blocks, UTXOs, scripts, fees, transactions, wallets, indexing translation |
| `sdk/chains/ethereum` | Ethereum addresses, RPC, blocks, gas/nonces, typed transactions, receipts/logs, wallets, indexing translation |
| `sdk/chains/solana` | Solana addresses, RPC, finalized sparse-slot blocks, System transfers, lamports, blockhash lifetimes, the send outbox, wallets, indexing translation |
| `sdk/wallets` | Chain-neutral capabilities, the wallet factory contract, history in display units, batch orchestration. It unifies factories and stores no wallet, no id and no key |
| `sdk/indexing` (root) | Storage-independent synchronization, the watch list it owns, canonical history, checkpoints, the confirmed line and the in-memory overlay above it, the store ports, the commit listener, the per-network health vocabulary |
| `sdk/indexing/src/storage/sqlite` (feature `sqlite`) | The durable indexing store: every table, statement and schema step, each whole operation on one pooled connection, off the executor |
| `sdk/indexing/src/storage/memory` (feature `memory`) | The second indexing store: every port on ordinary maps, for every network in one process. Not durable, and not a test double |
| `sdk/indexing/src/supervisor` (feature `supervisor`) | One Tokio task per composed chain: the per-network state machine, backoff, quarantine, the `SyncMonitor` callback, and the `SyncControl` resume handle |
| `sdk/indexing/tests/contracts` | The shared contract suites both stores are held against, as integration-test code rather than crate API |
| `sdk/config` | The configuration document: `Document`, `Registry`, `Sections`, `Chains`, plus the shell every chain's section shares (`Rpc`, `Repetition`, `Cadence`). It names **no chain crate**: a chain registers itself by implementing `sdk_config::Chain` for its own `Config`. TOML behind feature `toml`, off by default |
| `packages/*` | Generic crypto, HTTP, JSON-RPC, and storage mechanics usable outside blockchain projects |

`sdk/chains/*` holds concrete chain implementations only; chain-neutral crates
live at `sdk/<capability>` (`sdk/base`, `sdk/config`, `sdk/indexing`,
`sdk/wallets`). The
linter enforces both directories as closed.

Dependency rules:

- Packages do not import SDK or application crates.
- `sdk/base` does not import concrete chains, RPC, indexing, or wallets.
  It is not a universal transaction or RPC abstraction.
- `sdk/indexing`'s root knows no native block, stored record, wallet registry,
  Axum route, or business label; its `storage/sqlite` and `supervisor`
  modules are cargo features, so a dependent that wants only the contracts
  takes `default-features = false` and compiles neither. `Schedule` and
  `Schedules` sit outside the `supervisor` feature on purpose — a caller driving
  `sync` itself needs them, and they are a `Duration` and a map — so `wallets`,
  `sdk/config` and all three chain crates take `indexing` that way and feature
  unification never hands an embedder a store or a runtime it declined.
- Concrete chains retain chain-native semantics while implementing wallet and
  indexing contracts. Removing one chain leaves the other generic crates and
  other chain usable.
- `sdk/indexing/src/storage/sqlite` implements persistence only; it owns no
  synchronizer runtime and exposes no stored record through the crate's API.
- `apps/server` constructs the configured storage (one SQLite file), the one
  repository over it that serves every network, and all long-lived RPC clients,
  synchronizers, and wallet factories once. An asset never gets its own database,
  schema, pool, or repository. Handlers receive abstractions and do not open
  storage or construct dependencies per request.

Concrete chain crates follow `docs/CHAIN_LAYOUT.md` (worked example:
`sdk/chains/ethereum`), enforced by `linter.toml` and `scripts/check-chains.sh`:

```text
README.md  tests/{address_conversion,transaction_adapter,vocabulary}.rs
src/{lib,error,identity,source,interpreter,indexer,key,wallet,builder,sender}.rs
src/sender/{outbox,reservation,dispatch,recovery}.rs   the send pipeline (Outbox)
src/native/          protocol code only; never names indexing/wallets or SDK identities
```

Protocol-specific children may differ. Do not flatten cohesive directory
modules into oversized files.

Every chain is also demonstrated in every `examples/` scenario
(docs/CHAIN_LAYOUT.md section 8): one directory per scenario, one `main.rs`,
and one `async fn <chain>(…)` setup function per chain inside it. A chain crate
named anywhere else in a scenario fails `examples/tests/scenarios.rs`, and a
chain missing from a scenario fails it and `scripts/check-chains.sh` check 4.
Each chain has one chain key, `CHAIN_NAME: base::ChainName`.

## Domain models and collections

Models express payment and chain language, valid state, invariants, and
behavior. Put behavior on the value it primarily uses. Start with the smallest
meaningful entity and compose entities into collections and capabilities.

An entity owns one thing's identity and invariants. A plural type owns
operations over a meaningful collection. Prefer `Wallet`/`Wallets` and
`ValueMovement`/`Movements` to `WalletData`, `WalletManager`, helpers, or raw
vectors passed between free functions.

```rust,ignore
pub struct Wallets<K: Ord> {
    values: BTreeMap<K, Arc<dyn Wallet>>,
}

impl<K: Ord> Wallets<K> {
    pub fn insert(&mut self, key: K, wallet: Arc<dyn Wallet>) -> Result<(), Error> {
        // Enforce duplicate and identity rules here.
    }

    pub fn get(&self, key: &K) -> Result<Arc<dyn Wallet>, Error> {
        // Keep lookup policy with the collection.
    }
}
```

A domain collection must own cohesive construction, selection, validation,
aggregation, or transformation; it must not merely forward `Vec` methods.
Receipt-success rules, for example, belong to a collection constructor:

```rust,ignore
struct Movements(Vec<ValueMovement>);

impl Movements {
    fn successful(
        transaction: &ParsedTransaction,
        receipt: &ParsedReceipt,
        network: &Network,
    ) -> Result<Self, IndexError> {
        // Build native and token movements under one success policy.
    }

    fn into_vec(self) -> Vec<ValueMovement> {
        self.0
    }
}
```

The collection owns the result's meaning; an orphan
`successful_movements(...) -> Vec<_>` does not. Keep chain-specific collections
private to their chain unless a stable cross-chain contract needs them.

Use associated constructors for validation and policy:

```rust,ignore
let address = chain_bitcoin::Address::parse(input, network)?;
let indexer = chain_bitcoin::indexer(IndexerParams::new(rpc, repository))?;
let indexers = indexing::Index::of(vec![Arc::new(indexer)])?;
```

Use `From`, `TryFrom`, and `FromStr` for complete policy-free conversions. Use
named constructors for network, chain, wallet, fee, or storage policy.

Before preserving a primitive as state, inspect all assignments and
comparisons. Model closed vocabularies such as synchronization phase and
transaction status as enums; keep wire/storage encoding at the boundary.
Preserve unknown extensible protocol values when required. Identifiers,
messages, and user text are not closed enums.

When types duplicate the same identity and facts, compose one shared entity
into specialized nouns. Put shared behavior on the base and stage-specific
behavior on its wrapper. Expose deliberate access/conversion; do not use
`Deref`, field forwarding, or a trait to imitate inheritance. Similar field
names alone do not prove a shared entity.

Do not create a wrapper merely to turn one helper into a method. Decide whether
behavior belongs on an existing receiver/collection, a standard conversion, an
associated constructor, intentional inline code, or a genuine low-level or
multi-entity algorithm. Several cohesive rules and invariants establish a new
concept; one shared primitive argument does not.

## Capabilities and public APIs

Give each trait one capability and at most three tightly coupled methods.
Prefer standard traits and concrete code to ceremonial abstractions.

```rust,ignore
pub trait Addresser {
    fn address(&self) -> &base::Address;
}
```

Code needing an address or an asset accepts `&dyn indexing::Holder`, which a
balance and a history read both take. Use `dyn Wallet` only where the composed
capability is needed. `WalletFactory` constructs wallets; it is not a
post-construction wallet capability. History is not a wallet capability: the
index answers it by wallet address and asset (`Wallets::history`). Crate roots re-export small stable surfaces and keep implementation
modules private.

Keep logic and types close to their owner. Reusable wallet, indexing,
transaction, address, and storage models remain beside the domain behavior
that defines them. Do not move them into application `dto`, `models`, or
`common` namespaces.

### HTTP endpoints

Handlers are thin public adapters grouped by resource (`wallet`,
`transaction`, `health`, `contract`) and contribute to one Utoipa contract.
Define each endpoint's simple JSON input and output structs immediately above
the endpoint function so its contract is visible in one place. Do not scatter
endpoint-specific types through catch-all `request.rs`, `response.rs`, or
`dto.rs` files. Extract a shared wire type only when the exact same type is
genuinely used by multiple endpoints. Reusable domain models remain in their
owning SDK/domain module.

```rust,ignore
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct CreateWalletRequest {
    pub chain: Chain,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CreateWalletResponse {
    pub id: WalletId,
    pub address: String,
}

pub async fn create(
    State(wallets): State<Arc<Wallets<WalletId, Chain>>>,
    Json(input): Json<CreateWalletRequest>,
) -> Result<Json<CreateWalletResponse>, HttpError> {
    wallets.generate(input.chain).await.map(Json).map_err(Into::into)
}
```

HTTP owns extraction, authentication, limits, status mapping, and encoding.
Domain models own validation; a precise domain collection owns orchestration.
Axum handlers may remain free functions because the framework
owns their extractor signature, but their bodies stay to extraction, one
application call, error translation, and encoding. SDK crates know no Axum,
route, bearer token, or response shape. Secrets never derive serialization,
schema, or ordinary `Debug`.

## Transaction and wallet invariants

```text
chain-native request
  -> unsigned chain-native transaction
  -> chain computes payload/digest
  -> injected Signer signs it
  -> chain validates and inserts signature
  -> exact signed bytes are broadcast
  -> indexing observes inclusion, confirmation, failure, or reorg
```

- Never create a universal Bitcoin/Ethereum/Solana RPC or transaction model.
- Bitcoin owns network checks, outpoints, scripts, UTXOs, dust, checked
  satoshi fees/weight, sighashes, witnesses, and consensus encoding. History
  preserves every input/output movement. A batch may fund one transaction from
  several wallets, with deterministic fees and per-source change/signatures.
- Ethereum owns chain ID, signer recovery, nonce, gas, checked EIP-1559 fees,
  typed envelopes, token calldata, receipts, and logs. Native and token assets
  remain distinct and exact `U256` values are preserved. Batch transactions use
  consecutive per-source nonces, broadcast in input order, and report accepted
  prefix plus first failure.
- Solana owns Base58 addresses, Ed25519 seeds, slots, account/context floors,
  legacy System-transfer-plus-Memo messages, blockhash lifetime, fees,
  simulation, exact signatures, ordered broadcast, and source-keyed ambiguity
  coordination. It is configured with exactly one endpoint and every call is
  `json_rpc::Request::once`, so no transport decorator can repeat it; indexing
  remains the confirmation authority.
- A batch is one non-empty ordered list on one chain. Validate destinations,
  amounts, fee bounds, wallet compatibility, and chain invariants before the
  first external effect.
- Preserve exact signed bytes across ambiguous retryable broadcasts. RPC
  acceptance means submitted, not confirmed; RPC failure remains unknown and
  does not prove a drop. Confirmation comes only from indexing.
- Money uses checked atomic integers and exact `Decimal`, never floating point.

## Indexing invariants

- `Indexing` is the whole of what an index is — observe, advance, say where it
  got to — and every method on it is answerable by a `Repository` alone. There is
  no wider indexer trait above it, because **querying is the repository's
  responsibility and not the indexer's**. A consumer reads no query through an
  indexer: `History` is answered by `ObservedHistory`, which joins one
  repository's `ConfirmedHistory` pages to that index's overlay, a balance is
  `ObservedBalance` over that store's total completed by the same overlay, a
  spendable set is
  `ObservedOutputs` over the store's `OutputStore`, and a consumer serving
  several networks holds one of each per network
  (`wallets::Composition::history`, `::balance`). Each reader names its three
  dependencies — the repository, the blocks above its confirmed line, and the
  inspection progress a read is gated on — and `Service::history` /
  `Service::balances` / `Service::outputs` hand them over, because the service is
  the one value holding all three; none of them is an impl on the service. Bitcoin, Ethereum,
  Solana and the multi-chain `Index` all implement `Indexing`; callers do not
  change APIs when moving from one chain to several. `Index` and `Supervisor`
  are generic over the child trait with `dyn Indexing` as the default, so a
  store answering no query at all is still composed and supervised.
- A checkpoint contains native position, produced height, hash, and one atomic
  optional parent pairing parent position with parent hash. Native position
  drives traversal, canonical lookup, restart, readiness, and birthdays;
  produced height drives confirmation and ordering.
- The index owns what it observes. `Watching::watch(id, &dyn Holder, Start)`
  writes the durable `indexing::WatchStore` row and inserts into the hot set
  each `Service` keeps; `Indexing::sync` and `Supervisor::new` take no selection
  argument, because each unit reads its own list on every pass.
  The store is the mirror, read once before the first pass, not an authority
  a pass consults: `NetworkAdmission::plan` captures the list through a
  synchronous closure while `WatchStore::all` is async. One read, not two:
  `all` returns each watch beside its own `indexed_through`, from one snapshot,
  because a watch and how far it has been inspected are one fact and two reads
  could pair a watch with another registration's progress. A watch row
  holds the id, the address, the birthday and the `AssetId` it is read in, and
  **no key material** — observation is indexing's, custody is the
  application's. The set is keyed by watch identity, so one address is observed
  once per asset; scanning is deduplicated by address instead, from the earliest
  birthday any observation of it names, because `BlockInterpreter::inspect`
  takes addresses and no asset.
- **Progress is one contiguous, hash-verified block per observation.**
  `WatchProgress::indexed_through` means every applicable block from that
  observation's birthday through that block was inspected for it, empty ones
  included; `next_required()` is where it resumes. There is no second position
  and no gap, which is what one reference can honestly describe.
  `WatchProgress::caught_up(checkpoint)` is **the one predicate** behind both
  halves of the trade: an observation that has not reached the confirmed line is
  inspected for what it owes rather than for what is arriving now, *and* reads
  touching it are gated through `Completeness::while_backfilling`. Its doc comment
  states both possible behaviours and which is chosen; flipping the trade is that
  one comparison. Never add a second notion of "still catching up" beside it.
  `Watching::unwatch(id)` withdraws a watch: it
  stops inspection, deletes nothing, and is reversible, which is what replaced
  the refusal `docs/design/030 §8 (ex-029)` §5.2 recorded (`docs/SYSTEM_REQUIREMENTS.md` revokes that
  requirement in place). Nothing in `indexing` calls back into `wallets`.
- `Listener::committed(&BlockCommit)` is the one reaction hook: registered with
  `Indexing::listen` as a list rather than a slot, called once per committed
  block in block order after the store returned, synchronous, unable to fail
  and unable to veto. Delivery is at most once and never retracted. `IndexStore`
  is the veto-capable path and the answer to fully custom storage;
  a store is a constructor argument, never a runtime registration.
- Transactions preserve all stable movements. Never flatten a
  multi-input/multi-output UTXO transaction into a fictional transfer.
- Each chain states its own confirmed line (`ChainTip::confirmed_tip`): the
  newest block it will call irreversible. Ethereum asks its node for
  `finalized`, Solana already reads only rooted blocks and returns its tip, and
  Bitcoin, which has no finality, subtracts a configured depth. Nothing above
  that line is ever written.
- `Confirmed` is a fact, not a threshold: it means the durable store holds the
  transaction, which it only does at or below the line. Above it a transaction
  is `Pending`, reported apart on the first page. History cursors carry a
  position only: confirmed history is append-only, so a moved checkpoint can
  only have appended above a cursor and paging never conflicts.
- One commit atomically writes address-primary canonical history, live output
  changes, and checkpoint movement.
- A reorg above the confirmed line truncates the in-memory overlay: no store
  I/O, no backend code. A reorg below it is a terminal `ConfirmedReorg` and
  requires the caller to recreate and rescan only that exact network's
  indexing-owned rows; unrelated networks and application-owned tables remain
  untouched. That rescan is `Purging::purge_network`, exposed offline as
  `payment-server <config.json> --rescan <chain>/<network>`; it is the only
  port method that removes anything, it names the one network it purges, it sits
  outside `Repository` because the rescan is not indexing, and it must never
  start the listener or the supervisor.
- Persistence has **two required ports**, together `Repository`, and nothing
  else is asked of a store: `WatchStore` (`add`, `all`, `forget`, `reanchor`) is
  the durable
  watch list, and `IndexStore` (`checkpoint`, `commit`) holds the checkpoint and
  takes the **one** write a pass makes. The rest are optional
  consumer capabilities, each its own trait, so a store that answers no query is
  legal rather than stubbed: `BalanceStore` folds a balance, `OutputStore` lists
  live UTXOs, `Purging` owns `purge_network`. History is not a port at all —
  querying is the repository's own `ConfirmedHistory` API, joined to one index's
  overlay by `ObservedHistory`, so no sink carries a pagination model. None asks for `Clone`; each of them, and
  `ConfirmedHistory` too, has an `Arc<T>` forwarding
  impl, so a sink holding a connection or a channel is used as `Arc<MySink>`.
  Do not split load/plan/apply phases into ceremonial public traits, and never
  reintroduce a removal, an undo record, or a rollback journal.
- `IndexStore::commit` takes one `IndexedBatch` and atomically writes its
  history, its credits, its folded live-output changes, the `Progress` it proves
  and the line it leaves behind. **One write, both directions**: whether the
  checkpoint moves is `IndexedBatch::checkpoint`'s answer, derived from the
  blocks, so filling in a block the network already committed for an observation
  that was not watched at the time is the same call with the affected observation
  simply further behind. Every row is insert-if-absent, verify-if-present,
  terminal on one that disagrees. **Each `Progress` carries the `expected` value
  its author read, and the store compares it** — never a monotonic rule, because
  a batch redone under a smaller budget can honestly settle less than the write
  it is redoing, and refusing that would wedge the observation for good. A
  refusal is retryable and makes the index read its own list again.
  Chain interpreters emit facts;
  storage keys, codecs, records, and compare-and-swap mechanics remain private.

The synchronizer stores only canonical checkpoint, address-primary canonical
history, and current live outputs (the `WatchStore` capability owns its own
rows). The blocks above the confirmed line live in memory on the `Service` and
are rebuilt by the ordinary catch-up pass after a restart. Do not add
synchronizer status, observation revisions, spent markers, secondary address
indexes, event feeds, or raw-block archives.

The one marker that does exist is not a spend log: a required spend that matched
no live output is kept, exactly the keys a commit already reports, because the
block that created the output it named may still be filled in below the
checkpoint while the spend that named it was applied above where that filling
reaches. Nothing reads it but the create path of a later batch, which cancels the
create against it; only `purge_network` removes one.

The composition uses one SQLite file for every network, and there is no second
live store: the one capability a shared database would add is multi-writer
deployment, which `docs/SYSTEM_REQUIREMENTS.md` places out of scope.
`indexing::sqlite::Repository::new(database)` cannot fail and serves every
network: each operation names the network it is about, every table is keyed
`(chain, network, ...)`, and a read of a network holding no rows answers empty
rather than refusing. Sharing one file does not grant the indexing *runtime*
ownership of the watch list: a pass must not read, rewrite or remove the
registration table, which a network purge also leaves untouched. Only
`WatchStore` reaches it, through `add`, `forget` and `reanchor`, and a `Service`
reads it once, through `all`, before its first pass.

## Coding, errors, security, and concurrency

- Rust 2024, resolver 3, MSRV 1.91, locked dependencies. Workspace
  `unsafe_code = "forbid"` must not be weakened.
- Return typed errors from libraries and add actionable context at application
  and transport boundaries. Preserve retryability and ambiguous outcomes.
- Use `?`, checked arithmetic, and boundary validation. Avoid production
  `unwrap`, `expect`, and panic unless a local invariant makes failure
  impossible and evident.
- Parameterize external values; never interpolate untrusted data into commands,
  paths, keys, or URLs without canonical boundary encoding.
- Private keys never appear in logs, JSON, responses, snapshots, clones, or
  `Debug`; use zeroizing secret memory. Concrete chains accept injected
  `Signer`s and do not select custody policy.
- Never use funded keys or public broadcast endpoints without explicit user
  authorization for that exact external action.
- Borrow for observation; transfer ownership for storage/transformation. Clone
  only when ownership requires it. Use `Arc`, locks, channels, and atomics for
  their semantics, not by habit.
- A type with several construction inputs takes a parameters struct and builds
  itself from it: `Type::from_params(params)`. Never invert it into
  `params.type()` — a parameter bag does not construct the thing it configures,
  and the inverted form puts one constructor per type on a struct that should
  only carry values. This has been reverted once across three chains, three
  `config.rs` files, the tests, the examples, five READMEs and three documents;
  do not reintroduce it.
- Introduce concurrency only for real parallelism/latency. Do not block async
  executors or hold locks across `.await`; move blocking storage/CPU work off
  executors and bound spawned work.
- The `apps/server` composition root owns cancellation, fatal-task handling,
  readiness, and graceful shutdown. Do not hide unresolved chain composition
  inside a generic `Application` wrapper. The public listener starts only after
  every configured network has settled: reached `SyncPhase::Ready` with a
  persisted checkpoint at least once, or become `NetworkState::Quarantined`.
- One chain's indexing failure must not stop another's.
  `indexing::Supervisor` drives one task per `indexing::SyncUnit`
  and returns only on shutdown. A retryable failure retries that network with
  capped, jittered backoff; a non-retryable one, or a panicked unit task,
  quarantines that network alone, parks it with its last checkpoint, and is
  never auto-retried, because every class that reaches it is deterministic.
  `SyncControl::resume(&network)` is the way out when the cause was fixed out
  of band, reachable without a restart as `POST /v1/status/resume`;
  `ConfirmedReorg` re-quarantines within one pass and needs the
  offline rescan instead. `SyncMonitor` is a
  synchronous, notification-only callback: it fires on every state change and
  every failed attempt, never per catch-up batch, and must not do work.
  `Listener` is synchronous for the same reason, and a slow one costs latency
  on its own chain alone.
  Readiness is `all(Ready | Quarantined) && any(Ready)`; a quarantined chain
  never holds `/health/ready` at 503 forever, and all-quarantined answers 503
  without exiting.

## Delivery and style

- Reject unsupported meaningful input; never accept ignored configuration.
- Prefer mature protocol libraries over local standard reimplementations.
- Write a failing behavioral test for a defect or testable behavior. Test exact
  outcomes, edges, errors, and invariants.
- Unit tests stay beside their owner. Repository contract tests run every
  shared scenario against both shipped stores -- the SQLite file and
  `indexing::memory::Repository` -- as a pair of tests per scenario in
  `sdk/indexing/tests/repository_contract.rs`, so they stay statements about
  the ports rather than about either store. System tests compose the public facade,
  RPC doubles, the synchronizer, and one temporary index file. Tests are deterministic, own their
  resources, preserve sentinel registry rows, and never contact public networks.
- Indexing coverage includes birthdays, catch-up, per-observation contiguous
  progress including the `docs/design/030 §8 (ex-032)` X/Y/Z table as one test,
  checkpoint/hash restart,
  duplicate commit, a reorg above the confirmed line changing no stored byte,
  a reorg below it quarantining with `ConfirmedReorg`, confirmed and pending
  balance across a reorg, Bitcoin change spendable at depth one,
  native-position restart, produced-height confirmation,
  sparse Solana slots, Ethereum native/token movements, shared-schema network
  isolation, and RPC outage without false terminal state.
- Use precise nouns and plural collections. Avoid `Manager`, `Helper`, `Util`,
  `Impl`, vague abbreviations, repeated chain prefixes, and catch-all `core`,
  `common`, `shared`, `util`, or `misc` modules.
- A type is already a namespace: prefer `Wallets::insert`, `Index::history`,
  and `Address::parse_for_network` over repeated receiver names.
- Prefer receiver methods, `Display`, `FromStr`, `From`, `TryFrom`, and iterator
  traits when they express the contract. Newtypes must enforce an invariant,
  prevent meaningful mixing, own cohesive behavior, or stabilize a boundary.
- Keep the happy path shallow and matches exhaustive. Derive traits only when
  semantics are correct; inspectable non-secret domain values implement
  `Debug`.
- Comments explain contracts, security, compatibility, or non-obvious reasons;
  names explain mechanics. Keep public APIs and rustdoc focused.
- Every crate's `README.md` is its crate doc (`#![doc = include_str!("../README.md")]`)
  and follows a retired design note (005) section 3: why it exists, concepts, public API map,
  a compiling usage doc-test (`no_run` for I/O, never `ignore`), and related
  crates. `missing_docs` is denied workspace-wide: document every public item
  and update the README when its API changes.
- Production Rust is limited to 500 counted physical lines. Standalone test
  files and lines inside test-only Rust items/modules do not count. Production
  and tests must still be organized cohesively; numbered fragments and
  `include!` do not create boundaries.
- Refactor incrementally, search callers before renaming, and delete obsolete
  code rather than maintaining parallel designs.

Every addition must answer: which domain/folder owns it, is it close to that
owner, and does the public API remain simple for callers?

## MCP design-linter workflow

Use only the Linter MCP plugin and its `linter:software-design` skill for design
review. The local linter sources have been removed. Do not restore or recreate
an in-repository linter, invoke its former Cargo commands, or use its legacy
configuration, generated case queues, or suppression directives. This rule
supersedes older documents and remembered local-linter workflows.

Read the plugin skill before design work. Preserve the existing `linter.toml`;
it is the project policy for MCP checks. Call `mcp__plugin_linter_linter__check` with the
absolute repository root as `root`. Inspect both findings and each configured
rule's status: an unconfigured rule is not a pass, and an analysis failure is
not a clean result. MCP does not replace compilation, tests, rustfmt, or Clippy.

Start from a fresh MCP report after source or configuration changes. Before
editing, state the requested batch and its actual source owners. Review the
finding's rule, path, evidence, current source, callers, sibling behavior,
manifests, and tests. Findings are design-review signals, not proof of runtime
bugs; verify test-only scope and external protocol or framework boundaries.

For independent work, assign small, disjoint batches to subagents. Every agent
must read this guide and the plugin skill and preserve other agents' edits.
Prefer existing receivers or collections, standard conversions, associated
constructors, intentional inline code, or a genuine algorithm. Do not create
wrappers or split methods mechanically just to satisfy a numerical limit.
Report each case as `refactored`, `exception proposed`, or `blocked`, with its
owner, API impact, reasoning, files, and focused tests.

Subagents implement; the lead reviews every diff and approves any exception.
If no safe design is justified, leave the finding visible and explain the
missing evidence. A justified exception must use the MCP plugin's actual
syntax, an exact rule ID, and a concrete reason on the affected item. Do not
translate obsolete local-linter directives into MCP suppressions automatically.
Never add blanket suppressions, weaken limits, broaden exclusions, or change
policy merely to empty the report.

After each batch, review the diff and focused behavioral tests. After
integration, rerun MCP and confirm that selected findings disappeared without
introducing unrelated findings. Report configured-rule completion, remaining
findings, and blocked verification honestly. Keep user-requested batch limits;
do not proceed into another category or commit without authorization.

## Change workflow and validation

Inspect `git status --short`, preserve unrelated changes, read the owning
manifest/API/callers/tests, identify ownership, make one coherent change,
update public-contract docs, add focused tests, and report pre-existing failures
separately. Never weaken a gate.

Use locked dependencies. Workspace completion requires:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --doc
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
sh scripts/check-chains.sh
git diff --check
```

CI (`.github/workflows/check.yml`) runs all eight gates plus `linter check .`;
the whitespace gate runs there against the whole committed tree
(`git diff --check <empty tree> HEAD`).

Also run the Linter MCP `check` tool with the absolute repository root and
report configured-rule statuses and findings.

For Markdown-only changes, `git diff --check` plus path and terminology review
is enough. Public API, Cargo, transaction, signing, persistence, or
synchronization changes require focused crate tests followed by workspace
gates.

## Running a fleet

Keep roughly six agents busy when independent work exists. Give each a bounded,
non-overlapping crate or source area and continue manager work while they run.
Do not create overlapping edits merely to meet a number, but do not wait on one
agent while five useful independent tasks are available.
