# 035: The indexing boundary, cut and renamed

> "Its job is to inspect chains and persist facts."
>
> "History, balance and output queries belong to repositories: their
> calculations, pagination and response formats are not the indexing engine's
> responsibility."
>
> "Pending facts may be exposed without making the indexer a balance/history
> service."

Status: **working plan.** Steps are landing incrementally and the tree has
already moved since the first draft:

- Landed: both `#[path]` tricks are gone.
- Landed: the query vocabulary — `Page`, `HistoryQuery`, `ConfirmedHistory`,
  `Balance`, `Completeness`, `Ledger`, the output request/page types — now
  lives in `src/storage/{history,balance,outputs}.rs`. Root `output.rs`
  holds the committed facts only.
- Not yet landed: the joiners (`reads.rs`, `read_outputs.rs`,
  `ObservedHistory`/`ObservedBalance`/`ObservedOutputs`, the `Service` reader
  constructors), `Completeness::VerifiedOpening`, every rename in §3, every
  density fix in §4.

Where a section below speaks in the future tense about moved files, read it
against the status above. Paths are cited as they stand in the tree today.

The authority for the target state is `AGENTS.md` itself: the ownership table
now assigns `sdk/indexing` "watches, block inspection, continuity,
confirmation, pending blocks and atomic commit coordination", and
`sdk/indexing/src/storage/` "repository contracts/implementations and balance,
history and output queries". The code does not match that table yet. This
document is the plan for making it match.

---

## 0. The model: engine, storage, repository

**The goal of indexing.** Primitives and constructs that let a repository get
filled with consistent history. Watches. Block inspection. Continuity,
confirmation, pending blocks, atomic commits. Nothing else. Indexing is not a
balance service, not a history service, not an application.

Three parties, three rules:

- **The engine** (`src/` root). Primitives only. It fills a repository and
  keeps the history it writes consistent. It requires exactly one thing of a
  repository: `Repository` = `WatchStore` + `IndexStore`. If the engine ever
  needs a query or a list back from a repository, that interface is declared
  in `storage/` too — never in the root. The engine names only what it needs.
- **`storage/`.** Glue between the engine and storage. Intended only for
  storage. Holds:
  - the port definitions: `WatchStore`, `IndexStore`, the optional
    capabilities;
  - the query interfaces and their models: balance, history, outputs —
    repository-owned, engine-agnostic;
  - shared helpers the shipped stores use — e.g. the balance fold — which are
    glue for the default implementations, not a service anyone must call;
  - the two feature-gated default implementations: `sqlite`, `memory`.

  Nothing under `storage/` is application API. An application composes stores
  at the root; it does not import storage glue.
- **A repository implementation.** The defaults ship in-crate, gated
  (`sqlite`, `memory`). End-user storages — Postgres, anything else —
  implement the same interfaces their own way, outside this crate. Each
  storage owns its calculations: how a balance folds, how a page pages, is
  its business. Only `Repository` is required of every storage. Everything
  else is that storage's own choice. A storage offers a query capability by
  implementing the interface; it does not owe anyone the helper another store
  happened to share.

**`Ledger` under this model.** The fold is storage's business, full stop.
Today it sits in the engine root (`src/balance.rs:224`) because the balance
query layer does. After the cut it lands in `storage/` as shared glue the
shipped stores may use — or each store folds its own way, SQLite in SQL and
memory in maps. Either way: the engine never owns it, and a third-party
storage owes nothing about it. Same for pagination, response models, and
completeness claims.

**Where shared vocabulary goes: `sdk/base`.** `base` is the common place.
Chain-neutral values and contracts used across packages. The bar is high:

- It must be really generic. No chain knowledge, no storage knowledge, no
  indexing knowledge.
- It must touch more than one package. Chains and indexing. Stores and the
  API. The wider the better.
- It must have no other owner. If one package could own it, that package
  owns it.

Existing examples, all satisfying the bar: `Address`, `AssetId`, `Network`,
`ChainName`, the `BlockRef` family (`BlockHash`, `BlockHeight`,
`BlockPosition`, `BlockParent`), `MovementKind` (shared by chain movements
and indexed history — its doc says so), `Decimal`, `Units`, `Signer`,
`BoxFuture`. Indexing already re-exports these from `base` rather than
redefining them.

The failure modes, both banned:

- Storage glue promoted to `base` because two stores share it. Sharing
  between two implementations of one port is not cross-package genericity.
  The fold stays in `storage/`.
- A `base` abstraction built for one consumer. If only indexing uses it, it
  lives in indexing. If only stores use it, it lives in `storage/`.

---

## 1. The boundary: what the engine is

### 1.1 What stays in the engine root

These already match the requirements. No work beyond renames (§3).

- Observation: `Watch`, `Start`, `Watching`, the hot set, the admission
  handshake (`src/watch.rs`, `src/watches.rs`, `src/watch_set.rs`,
  `src/admission.rs`).
- Birthdays. Per-observation progress. Shared scanning (`Set::filters`,
  `WatchProgress::caught_up`).
- The chain boundary: `BlockSource`, `BlockInterpreter`, `ChainTip`,
  `IndexedBlock` (`src/source.rs`).
- The bounded pass: `pass.rs`, `catchup.rs`, `backfill.rs`, `scan.rs`,
  `fetch.rs`, `overlay.rs`.
- Continuity: confirmed line, pending blocks, restart hash verification,
  reorg handling (`ConfirmedReorg`, overlay truncation).
- The required ports: `WatchStore` + `IndexStore` = `Repository`
  (`src/storage.rs`, `src/storage/commit.rs`, `src/storage/watch.rs`).
- The commit hook: `Listener` / `BlockCommit` (`src/listeners.rs`).
- The pass answer vocabulary: `SyncStatus`, `SyncPhase`, `BackfillStatus`
  (`src/sync.rs`).
- Caller-controlled execution: `Indexing::sync`, `SyncPolicy`, `Schedule`.
  The Tokio runner stays an optional feature (`src/supervisor/`).
- The stores stay inside `storage/`: `storage/sqlite/`, `storage/memory/`.

### 1.2 What left the engine root

All of it was query service. None of it inspected a chain or committed a
fact. The cut has landed; the files now live under `src/storage/` (status
above).

- `balance.rs` (~373 production lines at audit time). Now
  `src/storage/balance.rs`.
  - `Ledger` is a balance calculation. Storage's business, per §0 — the
    engine never owns it.
  - `Completeness` is a response claim. Four variants, three narrowing gates.
  - `Effect`, `FeeSettlement` are fold policy.
  - The `Balances` trait fronts the join, not the engine.
  - `num-bigint` was in this crate only for `Ledger`. It now rides with the
    storage layer.
- `history.rs` (~374 lines). Now `src/storage/history.rs`.
  - `Page`, `HistoryPosition`, `HistoryQuery`: the pagination model.
  - `CanonicalPage`, `TransactionPage`: the response models.
  - `ConfirmedHistory`, `History`: query traits.
- `output.rs`, read half. Landed as `src/storage/outputs.rs`; the root file
  keeps `OutputId`, `OutputKey`, `IndexedOutput`, `OutputChanges` — the
  committed facts.
- `reads.rs` and `read_outputs.rs` (~550 lines + ~1,000 of tests).
  **Not yet cut** — still at the root.
  - `ObservedHistory`, `ObservedBalance`, `ObservedOutputs`: the joiners.
  - The `Completeness` gates on reads (`while_backfilling`,
    `without_a_birthday_proof`). The engine currently *repairs* repository
    answers from watch-set knowledge. The new rule: a repository reports
    incompleteness honestly from its own snapshot. The engine does not police
    it.
  - The pending fold (`PendingValue`), status derivation
    (`TransactionStatus`), confirmation arithmetic.
  - The revision-checked output cursor. Coin-selection machinery.
- The read models inside `observation.rs`. **Not yet cut.**
  - `ObservedTransaction`, `TransactionStatus` go with the cut.
  - `CanonicalTransaction`, `CanonicalStatus`, `ObservationDraft`,
    `ValueMovement`, `NetworkFee` stay. They are facts.

### 1.3 What replaces the joiners

One small seam, owned by the engine:

- The overlay stays (`overlay.rs`). `CanonicalBlock` stays public.
- The engine exposes pending facts: a sampled snapshot of the held blocks
  above a checkpoint. Nothing more.
- Consumers compose. Repository page/fold + pending snapshot. The composition
  is the consumer's job, per the requirement: "consumers needing
  pending-inclusive answers compose repository results with pending facts
  coherently."
- `Service::history()`, `Service::balances()`, `Service::outputs()`
  (`src/reads.rs:77-103`) are deleted. The indexer stops being the front door
  of a query service.

### 1.4 Already done — no work planned

- The two required ports are minimal. A store that answers no query still
  indexes. `tests/minimum_contract.rs` proves it.
- Query capabilities are already separate traits (`BalanceStore`,
  `OutputStore`, `Purging` in `src/storage/reads.rs`). Needing none is legal.
- `Schedule`/`Schedules` already sit outside the `supervisor` feature.
- The stores are already inside `storage/` and independently feature-gated.
- `ConfirmedHistory` is already absent from `Repository` — history querying is
  not a port.

---

## 2. Where the cut material goes

### 2.1 Destination: `storage/`, for storage only

- The query traits and their vocabulary move to `src/storage/` — the
  destination the ownership table already names. They become storage glue.
  Nothing under `storage/` is application API (§0).
- Suggested layout: `src/storage/queries/` for `history`, `balance`,
  `outputs`. The ports file (`storage/reads.rs`) either merges in or keeps
  naming the traits.
- Each storage answers the interfaces its own way. SQLite folds in SQL.
  Memory folds in maps. How a store calculates is its own business; the
  interfaces only name the questions.
- The `Ledger` fold lands in `storage/` as a shared helper the shipped stores
  may use — or each store folds its own way. Either is fine. What is not
  fine is the fold living in the engine root, where every consumer sees it as
  indexing's own machinery.
- Anything in the moved vocabulary that is genuinely cross-package generic —
  chains and stores and the API could all name it — goes to `sdk/base`, not
  to storage glue. Apply the §0 bar item by item. Nothing in the current
  query layer obviously clears it; that is a finding, not a gap.
- Contract suites follow their subject (`tests/contracts/history.rs`,
  `balance.rs`, `totals.rs`, `witness.rs`, `spends.rs` stay with the stores as
  repository contracts). Engine suites (`watch.rs`, `backfill.rs`,
  `rollback.rs`, `replays.rs`, `anchors.rs`, `purge.rs`, `chunk.rs`,
  `provenance.rs`) stay with the engine — see §3.7 for renames.
- The interleaved-commit read tests in `src/reads.rs` (~1,000 lines) become
  tests of the consumer composition, not of the engine.

### 2.2 The `VerifiedOpening` problem

- `Completeness::VerifiedOpening` exists for one chain feature: Ethereum's
  `BalanceWitness` (`sdk/chains/ethereum/src/wallet.rs:10,291`).
- That feature mutates `indexing::Balance` and writes the variant no store can
  persist.
- After the cut, the witness is a query-layer concern. The variant leaves the
  engine's vocabulary with it.
- The Ethereum chain crate stops importing `Balance`/`Completeness`.

### 2.3 Consumers to rewire

- `apps/api/src/api/history.rs`. 41+ direct references to indexing response
  types (`TransactionPage`, `ObservedTransaction`, `TransactionStatus`,
  `MovementKind`, `NetworkFee`, `Page`, `HistoryPosition`, `Completeness`).
  The HTTP wire layer maps storage-owned query results + pending facts to its
  own DTOs.
- `sdk/wallets/src/composition.rs`. Holds `Arc<dyn indexing::History>` and
  `Arc<dyn indexing::Balances>` per network. `sdk/wallets/src/lib.rs:17`
  re-exports `indexing::{Holder, Page, Subject}`. The composition-plus-pending
  join moves here or into a small query crate.
- `sdk/chains/ethereum/src/wallet.rs`. `BalanceWitness` re-homed (§2.2).
- `sdk/indexing/README.md`, `docs/INDEXING.md`, `ARCHITECTURE.md`. The public
  contract docs rewrite with the code.
- `sdk/indexing/examples/bench.rs` and `examples/custom_store/` follow the
  moved API.

---

## 3. The naming audit: what to rename

Every item: current name, location, why it fails, target. Renames are pure
mechanical replacements; no behavior change. Search callers before each one.

One principle governs the order (§5): **cut first, rename what survives —
at its destination.** A file that moves under `storage/` in step 2 is moved
with its names intact and renamed in step 3, after the move. Code scheduled
for deletion (`ObservedHistory`, the `Completeness` gates, the `Balances`
join trait) is never renamed at all: it dies with its bad name, which is
cheaper than teaching a new name to code with days to live.

### 3.1 Names that say nothing

| Current | Location | Problem | Target |
|---|---|---|---|
| `Service` | `src/service.rs:41` | The one-network indexer, named like an SOA service. Chains alias it `Indexer` to compensate. | `Indexer`. Chain aliases deleted. |
| `Set` | `src/watch_set.rs:25` | The hot watch collection. Reads like std vocabulary. | `Rows` — or the type disappears if step 1 folds it into `Watches` (§3.6). |
| `Start` | `src/watch.rs:27` | Birthday enum. `indexing::Start` reads like runtime lifecycle. | `Birthday`. Variants `Fresh`/`At` stay. |
| `Outcome` | `src/pass.rs:36` | The pass's commit tally `{checkpoint, unknown_spends}`. | `CommitTally` or fold into `SyncStatus` assembly. |
| `apply` | `src/pass.rs:421` | The commit lifecycle named like a serde method. | `commit_batch`. Siblings: `commit` → `commit_held`, `extend` → one-block step inside. |
| `Effect` | `src/balance.rs:187` | Credit/Debit/None. Physics-flavored. | `MovementEffect`. |
| `Ledger` | `src/balance.rs:224` | Not a ledger. A BigUint running accumulator. | `Fold`. Matches `PendingValue::fold`, `LiveChanges::fold`. |
| `facts` (field/var) | `src/overlay.rs:23`, `src/storage/commit.rs:163`, test helpers | Universal datum word. Names nothing. | `block` / `canonical` per site. |

### 3.2 One concept, several names — collapse to one

| Concept | Names today | Target |
|---|---|---|
| Blocks above the confirmed line | module `overlay`, type `Frontier`, field `Service.pending`, `HeldBlock` | `PendingBlocks` everywhere. Module `pending`. |
| Spends the store could not match | `tracked_spends` (input) vs `unknown_spends` (output) | `unmatched_spends` on both sides. |
| Cancel a backfill | `Backfilling::cancel` (engine) vs `WatchStore::reanchor` (store) | One verb: `cancel_backfill` on both sides. |
| Progress shapes | `WatchProgress`, `Progress`, `Backlog`; identity field `watch: String` | Keep `WatchProgress` (durable) and `Progress` (claim). Rename `Backlog` → `OwedRange`. Rename identity fields `watch` → `watch_id`. |
| Re-interpret held blocks | `reinspect` calls `recover` which usually cache-hits | `reinspect` stays; `recover` → `refetch`. |
| Native raw block | `IndexedBlock` (trait), `NativeBlocks` (cache), `recover` | `NativeBlock` for the trait. Cache `NativeBlocks` stays. |

### 3.3 Names that mean something else than what they are

| Current | Location | Problem | Target |
|---|---|---|---|
| `IndexedBlock` | `src/source.rs:11` | The RAW chain-native input. Reads as "already indexed". | `NativeBlock`. |
| `IndexErrorKind::CannotConnect` | `src/error.rs:49` | Means "blocks do not link to the checkpoint (reorg in flight)". Also used for a source lying about content (`src/fetch.rs:131-144`). Name screams RPC failure. | `Disconnected` for fork-in-flight; `InvalidBlock` for a lying source. Split the two. |
| `Watches::stale()` | `src/watches.rs:80` | Verb named like an adjective. It marks the mirror stale. | `mark_stale`. |
| `ChainTip` | `src/source.rs:25` | States TWO lines (`tip`, `confirmed_tip`). | `ChainTips`. |
| `NetworkAdmission` | `src/admission.rs:115` | The commit/publication serializer. Metaphor needs a paragraph. | `CommitGate` or `PublicationGate`. |
| `AddressFilter` | `src/watch.rs:14` | A work item (address + start), not a predicate. | `Coverage` — the word `admission.rs` already uses for the same map. |
| `FeeSettlement::Movements` | `src/balance.rs:175` | Noun beside `Charged`. Does not parse. | `FeeSettlement::Embedded`. |

### 3.4 Classification burden — collapse the multiplies

- **Status enums.** `ObservationDraftStatus` and `CanonicalStatus` have
  identical variants; only the block attachment differs
  (`src/observation.rs:205,281,438`). Keep `CanonicalStatus`. Make the draft
  carry it. `TransactionStatus` stays as the read-time status.
- **History vocabulary.** After the cut: `ConfirmedHistory` (repository
  query), `History` (pending-inclusive trait), `HistoryQuery`, `Page`,
  `HistoryPosition`, `CanonicalPage`, `TransactionPage`. One module, one
  file. No `Observed*` prefix on join values — the join type names its two
  halves (`ConfirmedWithPending` is a candidate; decide at implementation).
- **State vocabulary.** `SyncPhase` (one pass's answer) and `NetworkState`
  (standing condition) keep their split — the distinction is real. But
  `NetworkHealth` must not be `SyncStatus` with five copied fields
  (`src/health.rs:29-39`). Compose: `NetworkHealth { state, failure,
  attempts, since, status: SyncStatus }`. Wire-visible; `docs/API.md` and
  `openapi.json` update together.
- **`SyncHealth::settled`.** A latch, named like a predicate. Rename
  `ever_settled` / `reached_ready_once`. Doc comment states latch semantics.
- **Index family.** `Indexer` (type, after §3.1), `Indexing` (trait) →
  reconsider one name. `SyncUnit` → `NetworkIndexer` or fold into `Index`'s
  internals. `Index` stays the many-network value.
- **Progress fields.** `Progress.watch: String` and `Backlog.watch: String`
  hold ids. Rename `watch_id` (§3.2).

### 3.5 Sentence-names

- `Completeness::while_backfilling`, `without_a_birthday_proof`,
  `without_a_reported_amount` (`src/balance.rs:107-159`). After the cut these
  gates live in the query layer. Rename to what they check:
  `weaken_during_backfill`, `unproven_birthday`, `amountless_read`.
- `Set::observed_from_the_first_block` (`src/watch_set.rs:133`) →
  `born_at_genesis`.
- `Completeness::Overdrawn` stays — it is the fold's own proof, and the name
  reads correctly once `VerifiedOpening` has left.
- `PendingValue::fold` impl in `overlay.rs` (`src/overlay.rs:266`). After the
  cut the pending fold belongs to the consumer composition. The three-way
  split (type in `history.rs`, fold in `overlay.rs`, trait in `History`)
  disappears with it.

### 3.6 Structural tricks to remove

- `#[path = "read_outputs.rs"] mod outputs;` inside `src/reads.rs:31`. Merges
  into the query module when the joiners move.
- `#[path = "watch_set.rs"] mod set;` inside `src/watches.rs:16`. Make
  `watch_set.rs` a real module or fold `Set` into `Watches`. The
  check-chains single-file-directory rule does not apply inside
  `sdk/indexing`; the engine should not carry include tricks to satisfy a
  chain-layout script.
- Five watch files (`watch.rs`, `watches.rs`, `watch_set.rs`,
  `storage/watch.rs`, `WatchProgress` in `storage/watch.rs`) become three:
  `watch.rs` (row, `Start`, `Watching`), `watches.rs` (collection +
  admission), `storage/watch.rs` (port + `WatchProgress`).

### 3.7 Contract-suite names

Third parties are sent to these files by name (`sdk/indexing/README.md`).
Rename to what they guarantee:

| Current | Meaning | Target |
|---|---|---|
| `anchors.rs` | re-anchor behavior | `reanchor.rs` |
| `provenance.rs` | what a store may claim about the range behind its rows | `completeness_claims.rs` |
| `witness.rs` | completeness narrowing on pages | `page_claims.rs` |
| `chunk.rs` | shared backfill fixture | `backfill_fixture.rs` |
| `replays.rs` | idempotent replay | `idempotent_replay.rs` |
| `totals.rs` | authoritative balance total | `balance_totals.rs` |
| `spends.rs` | required-spend accounting | `unmatched_spends.rs` |

---

## 4. Density: one thing, more uses

A duplication audit of the tree as it stands now — query vocabulary already
in `storage/`, joiners still at the root. Four real duplications, then the
look-alikes that must stay separate.

### 4.1 The completeness rule — two copies, one rule

- `memory::Repository::provenance` (`src/storage/memory/mod.rs:389`).
- `sqlite`'s `claimed` (`src/storage/sqlite/balance.rs:145`), shared by both
  sqlite reads.
- Same pure function: `inspected = indexed_through.position >= birthday`;
  `CompleteHistory` iff `birthday == 0 && inspected`.
- Both docs warn that a second copy could disagree. The warning should be
  structural, not verbal.
- **Fix:** one function beside `WatchProgress` in `src/storage/watch.rs` —
  `Completeness::claimed(birthday, indexed_through)` or
  `WatchProgress::holds_from_first_block`. Both stores call it.
- Soundness: a pure function of watch-row facts. No backend framing, so both
  backends can reach it.

### 4.2 The `limit + 1` page probe — four copies, one mechanic

- memory `history_page` (`src/storage/memory/mod.rs:207`), `output_page`
  (`mod.rs:288`).
- sqlite `src/storage/sqlite/history.rs:93`, `outputs.rs:73`.
- Same shape: fetch `limit + 1`; `has_more = len > limit`; truncate;
  `next = last if has_more`.
- **Fix:** one helper in `src/storage/history.rs` —
  `fn page<T>(items: Vec<T>, limit: usize, to_cursor: impl Fn(&T) -> C) -> (Vec<T>, Option<C>)`.
  Row fetching stays per store. Only the probe mechanic shares.
- SQLite's `MIN_CHUNK` stays: a fetch bound, not the probe.

### 4.3 The block-linkage rule — two copies, one rule

- `Scan::follows` (`src/scan.rs:64`).
- `CanonicalBlock::connects_to` (`src/block.rs:253`).
- Same three comparisons: `position > previous`, `height == previous + 1`,
  `parent == (previous.position, previous.hash)`.
- One is read-time validation. One is batch validation. Two copies of one
  consensus rule drift.
- **Fix:** one crate-private predicate, `extends(block, previous) -> bool`.
  Call sites keep their error messages. The genesis special-case stays in
  `Scan`.
- Not `base/`: both users are inside indexing today.

### 4.4 The `canonical_at` disagree-check — three call sites

- `pass.rs` `confirm_checkpoint`. `pass.rs` `unwind`. `scan.rs` `admit`.
- Same shape: fetch `canonical_at(position)`, compare, `cannot_connect`.
- **Fix:** `Service::require_canonical(&self, block, message)`. One place to
  reason about reorg-in-flight handling.

### 4.5 Considered and declined — the look-alikes that stay separate

- **Three folds**: `Ledger::apply`, `PendingValue::fold`,
  `LiveChanges::fold`. Same input shape. Deliberately different policies:
  netting, fee settlement, output survival. A shared abstraction would
  outgrow the three loops. Concrete code wins.
- **The two pass directions**: `catch_up` / `fill`. Scheduling is
  deliberately per-direction (forward gets the budget first). Shared
  mechanics are already extracted: `inspect`, `apply`, `Scan`.
- **`WatchProgress` / `Progress` / `Backlog`**: different lifecycle moments —
  stored row, commit claim, owed range. Merging smears scheduling into
  claims.
- **`owns()`** (network+address predicate): memory spells it in Rust, sqlite
  in `WHERE`. SQL and maps cannot share it.

---

## 5. Work order

Ordered so every step leaves the workspace green. One step per change set.
The governing rule: cut first, rename survivors after, at their destination
(§3).

1. **Structure** (§3.6). Remove both `#[path]` tricks. Decide `Set`'s fate:
   fold it into `Watches`, or make `watch_set.rs` a real module. **Landed.**
2. **The cut** (§1.2, §2.1). Query vocabulary under `storage/` — **landed.**
   Remaining: delete the joiners (`reads.rs`, `read_outputs.rs`), the
   `Service` reader constructors, the `Completeness` read gates; expose the
   pending snapshot. Deleted code keeps its old name to the end. May land as
   two (history+models, outputs) in that order.
3. **Renames of survivors, at their destinations** (§3.1, §3.3, §3.5).
   Engine files: `Service` → `Indexer`, `Start` → `Birthday`,
   `IndexedBlock` → `NativeBlock`, `ChainTips`, the `reanchor` alignment,
   `mark_stale`. Storage files, now at their new home: `Ledger` → `Fold`,
   `Effect` → `MovementEffect` — and only if the fold survives as shared glue
   (§2.1); otherwise the old names leave with the per-store implementations.
   Chain-side aliases follow. Update READMEs and `ARCHITECTURE.md` in the
   same change set.
4. **Density** (§4.1–§4.4). The shared completeness rule, the page-probe
   helper, the linkage predicate, `require_canonical`. One commit each.
   Both stores stay green against the shared contracts after each.
5. **Rewire consumers** (§2.3). `wallets::Composition`, `apps/api` routes,
   Ethereum `BalanceWitness`. Delete `Completeness::VerifiedOpening` and the
   engine's balance imports from the chain crate.
6. **Split `CannotConnect`.** Fork-in-flight gets `Disconnected`; a source
   lying about content gets `InvalidBlock`. Touch `error.rs`, `fetch.rs`,
   `block.rs`, the supervisor's quarantine mapping if it switches on the kind.
7. **Collapse the state duplication** (§3.4): `NetworkHealth` composes
   `SyncStatus`; `settled` → `ever_settled`. Wire contract updated in the
   same commit.
8. **Rename the contract suites** (§3.7) and the test runner pairings.
9. **Final pass.** `sdk/indexing/README.md` rewrite (its API map and
   lifecycle currently lean on the readers). `docs/INDEXING.md` terminology
   sweep. Linter MCP check. Eight gates.

## 6. Explicitly out of scope

- **No behavior change to the pass, the ports, or the stores.** This document
  moves and renames. It does not change what a commit carries, what a store
  owes, or what a pass inspects.
- **The persisted-balance direction** (`AGENTS.md`: "repository target:
  persist absolute balances") is a separate design. This document keeps
  derive-on-read and only moves where the derivation lives: into `storage/`,
  as each store's own business (§0).
- **`BlockPosition` vs `BlockHeight`.** Two height-ish fields on `BlockRef`.
  The distinction is protocol reality (native position drives traversal;
  produced height drives confirmation). Renaming cannot fix it; better
  doc-comments only.
- **Merging `SyncPhase` and `NetworkState`.** They answer different questions
  (one pass vs standing condition). Only the field copying between their
  structs is fixed (§3.4).
- **No new query features.** No new ports, no new reader capabilities, no
  event feed. The cut shrinks the surface.

## 7. Verification

Per step:

- `cargo fmt --all -- --check`
- `cargo check --locked --workspace --all-targets`
- `cargo test --locked --workspace`
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `cargo test --locked --workspace --doc`
- `RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps`
- `sh scripts/check-chains.sh`
- `git diff --check`
- Linter MCP `check` with the absolute repository root; report configured-rule
  statuses and findings honestly.

Behavioral proof required for the cut (step 2) specifically:

- `tests/minimum_contract.rs` still passes with the query layer gone from the
  engine root — a store answering no query still indexes.
- Both stores pass every moved contract suite (`sqlite` and `memory` as a
  pair).
- One new test: the engine's pending snapshot is the same `CanonicalBlock`s
  the next commit writes. Pending facts exposed, no query service implied.
