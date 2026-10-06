# 046: `models/` — the server's entities get a home

> "find for me models … a folder with models/ where we represent entities
> used in apps/server (not encoding such as http io) but generally models
> server uses and its related storage in sql … `networks` sounds like bs."

Status: **implemented differently.** `apps/server/src/models/` exists, one file per noun: `wallet` (the `wallet` table: label, archive time, optional secret) and `endpoint` (`RpcEndpoints`, the `rpc_endpoints` table) with their SQL, plus `asset`, `block`, `completeness` and `network` without tables. Models carry their own serde/utoipa wire form (no `src/http/`), and `scripts/check-chains.sh` check 8 forbids axum, chain crates, `sdk_config` and `crate::api`/`crate::chains` there. `Registration`, `Node`, `Section`, `Rescan` and `networks/` do not exist: live node state is `api::networks::LiveNetwork` over a `json_rpc::Swap`, the rescan is `SyncControl::rescan` behind `Wallets::pause`, and the tables are `schema.sql` without step-sets. `apps/server/src/models/` becomes the home of the
application's durable entities: the validated type, its SQL row codec, and
its store, one file per entity. Everything durable today scattered across
`registry.rs` and `networks/{section,nodes,rescan}.rs` moves there. The
`networks/` module keeps only what is genuinely live orchestration state —
and stops being a junk drawer. Prerequisite: `045` (the primitives the
stores are written on).

---

## 0. What moves, what stays

**Moves to `models/`** — durable entities, each with its table:

| Entity | From | What it is |
|---|---|---|
| `Registration` | `registry.rs` | One wallet registration: id, asset tag, encrypted secret material |
| `Section` | `networks/section.rs` | One chain's durable settings row: networks, requested run/pause, resume counter, revision |
| `Node` | `networks/nodes.rs` | One configured RPC node — today a `Vec<Value>` bag; becomes a typed, validated entity |
| `Rescan` | `networks/rescan.rs` | One rescan/reindex request with its persisted phase |

**Stays in `networks/`** — live state, no SQL: `Admissions`, `Endpoints`,
`Upstream`, the plan/members/parts actor machinery. With the durable rows
gone, the name is honest again: the live actors of one network's lifecycle.
(If you still hate the name after the split, rename then — not before. The
name wasn't the problem; the mixture was.)

**Untouched:** `config.rs` (the static TOML document reader), `http/`
(wire DTOs and handler state — encoding, which explicitly is not models),
`signers.rs` (in-memory key map), `asset.rs` (tag newtype), `chains.rs`
(registration table), `operator.rs`, `ui.rs`, `devnet.rs`.

## 1. The shape of one model file

```rust
// models/registration.rs
pub(crate) struct Registration { /* private fields, validated constructor */ }
impl Registration {
    pub(crate) fn new(...) -> Result<Self, Error>;
    fn encode(&self) -> Row;          // to SQL parameters
    fn decode(row: &Row) -> Result<Self, Error>;
}
pub(crate) struct Registrations { pool: packages_sqlite::Pool }
impl Registrations {
    pub(crate) async fn save(&self, item: &Registration) -> Result<...>;
    pub(crate) async fn by_id(&self, id: &str) -> Result<Option<Registration>>;
}
```

- The entity owns its invariants at construction. The codec is private to
  the model. The store's methods are whole operations — one `pool.write` or
  `pool.read` each, never a leaked connection.
- No HTTP types in `models/`: no axum, no utoipa, no wire DTOs. `http/`
  maps *from* models at the boundary; the mapping direction never reverses.
- No chain types in `models/`: a `Section` stores chain ids and network
  names as text, exactly as it does today.
- One file per entity; entity + codec + store together while the file
  stays under the 500-line rule, split by responsibility after.

## 2. The `Node` decision — the concrete BS, removed

`networks/nodes.rs` today is `struct Nodes(Vec<Value>)`: an unvalidated
JSON bag that every consumer interprets separately. The model replaces it:

```rust
pub(crate) struct Node {
    alias: String,          // the opaque id healthz may publish
    endpoint: Endpoint,     // packages/json-rpc's validated type
    // capabilities and limits land here as fields when a policy needs them
}
```

- **D1 — storage shape.** Default: keep the existing JSON column; the typed
  entity validates at the boundary and the schema does not migrate.
  Normalizing to columns waits for a query that needs it.
- Verification, failover order, and statistics stay with the `networks/`
  actors; `Node` is the durable fact about one configured node, not the
  live connection.

## 3. Storage ownership after the move

- `storage.rs` keeps one job: resolve the path, open the one
  `packages_sqlite::Pool` (045), hand clones to `indexing::sqlite` and to
  each model's store. `Store(indexing::sqlite::Database)` is already gone
  in this world; the server never names the indexing backend.
- Each model's table is its own step-set in the same migration chain:
  indexing's steps, then registrations, sections, nodes, rescans. Owners
  stay separate over one pool — the structural echo of "a network purge
  never touches application tables."

## 4. Work order

One step per change set; every step leaves the workspace green.
`045` lands first — the stores are written on its primitives from day one.

1. **`models/registration.rs`.** Move `Registry`'s row entity and store;
  `registry.rs` becomes the module path or disappears into the model.
  Restart-roundtrip test moves with it.
2. **`models/section.rs`.** From `networks/section.rs`; the `networks/`
  actors consume it instead of owning it.
3. **`models/node.rs`.** The typed `Node` replaces `Nodes(Vec<Value>)` at
  every boundary; D1 keeps the column, so this is constructor + codec +
  call sites, no migration.
4. **`models/rescan.rs`.** From `networks/rescan.rs`, phase persistence
  included.
5. **Boundary pass.** `http/` and `networks/` import models; nothing
  reverse-imports; a `check-chains`-style grep test pins that `models/`
  names no axum, utoipa, or chain crate.
6. **Docs.** `ARCHITECTURE.md` gains the `models/` row; the server README
  states the three layers: `models/` (entities + SQL), `networks/` (live
  actors), `http/` (encoding).

## 5. Verification

The eight gates from `AGENTS.md`, plus the Linter MCP `check`.

- **Roundtrips.** Each model: save → reopen → identical entity, on a real
  temporary file, for all four.
- **Node validation.** Every malformed node document the old bag accepted
  silently is now refused at the constructor, with a test per refusal.
- **Boundary.** The grep test of step 5 fails the build if a wire type or
  chain crate enters `models/`.
- **No drift.** Restart tests for indexing, registrations, and settings
  still pass unchanged — entities moved, bytes did not.

## 6. Out of scope

- **No behavior change.** This is re-ownership: bytes, tables, and
  operation shapes are identical; only which module owns them changes.
- **No schema normalization** beyond D1. Columns for nodes, secrets, or
  settings wait for a query that needs them.
- **No models for live state.** `Admissions`, `Endpoints`, `Upstream`, and
  the actor plan stay in `networks/` — they have no SQL, so they are not
  models by this document's definition.
- **No moving `http/contract.rs`.** Wire DTOs are encoding; the boundary
  between wire and model is exactly what this document draws.
