# 045: `packages/sqlite` — the generic SQLite primitives leave indexing

> "there is lots of things in sqlite and in apps/server that is related to
> sqlite … generic core such as Pools, lease things that are generic to
> sqlite but not tight to indexing be extracted in packages/sqlite … I want
> this package to use in completely unrelated project … then you can reuse
> these generic things in server … `pub(crate) struct Store(indexing::sqlite::
> Database)` doesn't sound to me architecture-wise as good idea."

Status: **superseded by sqlx.** `packages/sqlite` is deleted; `indexing::sqlite` runs on `sqlx` (`Database::open`/`open_with`/`pool`), with one schema per owner applied to an empty file and no migrations.
A new `packages/sqlite` crate owns the generic
SQLite machinery that today lives inside `sdk/indexing/src/storage/sqlite/`:
the bounded pool, the per-operation lease, the off-executor runner, the
versioned migration step-runner, and the write/read transaction scopes.
`indexing::sqlite` keeps its domain — tables, statements, repository — and
becomes a consumer. `apps/server` opens one pool for its one file and builds
its own tables (registry, settings) on the same primitives. The server's
storage stops importing the indexing backend entirely.

---

## 0. The classification

From `sdk/indexing/src/storage/sqlite/mod.rs` (+ `schema.rs`), the module
today mixes two kinds of code:

**Generic — moves to `packages/sqlite`:**
- `Pool` — bounded connection lending (`idle`/`outstanding`/`Condvar`),
  one file, N connections, "past the machine's parallelism another reader
  buys memory, not concurrency."
- The lease/checkout guard — one connection borrowed for the length of one
  whole operation.
- The off-executor runner — every operation runs on a blocking worker via
  `spawn_blocking`.
- Write/read scopes — one `BEGIN IMMEDIATE` transaction per write
  operation; snapshot reads on separate pooled connections under WAL.
- The migration machinery — ordered steps applied inside the transaction
  that bumps `PRAGMA user_version`; a file this build cannot account for
  is refused, not interpreted.
- Pragmas on open (WAL, busy timeout) and the busy/locked error
  classification (`ErrorCode` mapping).

**Indexing domain — stays in `sdk/indexing/src/storage/sqlite/`:**
- Every table definition and statement (`tables.rs`, `blocks.rs`,
  `movements.rs`, `effects.rs`, `balance.rs`, `history.rs`, `outputs.rs`,
  `progress.rs`, `watches.rs`, `activity.rs`).
- The content of the migration steps — the indexing schema itself.
- `Repository` and every port impl. `Database` becomes a thin handle over
  the package's pool plus the indexing schema.

**Application domain — newly on the package, in `apps/server`:**
- The wallet registry table and the settings table get their own schemas
  expressed through the same primitives, on the same pool, in the same
  file. One SQLite instance serves indexing and the application — the
  `038` rule — without the application importing indexing's backend.

## 1. The package

```rust
// packages/sqlite — primitives only; knows no indexing, wallets, or server.

pub struct Options { pub ceiling: NonZeroUsize, pub busy_timeout: Duration, … }

pub struct Pool { … }                 // open file + bounded lending
impl Pool {
    pub fn open(path: &Path, options: Options) -> Result<Self, Error>;
    pub fn lease(&self) -> Lease<'_>;             // one operation, one connection
    pub async fn run<T>(&self, work: impl FnOnce(&Connection) -> Result<T, Error> + Send) -> Result<T, Error>; // spawn_blocking
    pub fn write<T>(&self, work: impl FnOnce(&mut Transaction) -> Result<T, Error>) -> Result<T, Error>; // BEGIN IMMEDIATE
    pub fn read<T>(&self, work: impl FnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error>;
    pub fn migrate(&self, steps: &[Step]) -> Result<(), Error>; // user_version, transactional, refuse-unknown
}
pub struct Step { pub version: u64, pub apply: &'static str or fn(&Transaction) -> Result<(), Error> }
pub struct Error { … }               // busy/locked retryability, sanitized display
```

- Sync primitives, async only in `run` (the off-executor helper). An
  unrelated project uses exactly this and nothing more.
- `rusqlite` types appear at the boundary (`Connection`, `Transaction`) and
  are re-exported, so consumers write closures without adding a direct
  rusqlite dependency of their own.
- No query builders, no ORM, no table opinions. Leases, scopes, migrations,
  pragmas, error classes. That is the whole package.

## 2. What changes where

- **`sdk/indexing/src/storage/sqlite/`.** `Database` becomes
  `Database { pool: packages_sqlite::Pool }` with `open(path)` and
  `from_pool(pool)` constructors. Domain modules keep their SQL but reach
  connections through `pool.write` / `pool.read` / `lease`. The migration
  steps move their *content* here and run through `pool.migrate`. No
  behavior change: the contract suites against both backends are the proof.
- **`apps/server/src/storage.rs`.** `Store(indexing::sqlite::Database)`
  dies. The server opens one `Pool` for its one file, hands a clone to
  `indexing::sqlite::Database::from_pool`, and opens its registry and
  settings tables on the same pool with their own steps. The server's
  storage module names a path and a pool — not an indexing type.
- **`apps/server/src/registry.rs` / settings.** Their schemas become
  `packages_sqlite` steps beside the indexing steps; their operations use
  `pool.write`/`read`. Ownership stays application-side.

## 3. Boundary rules

- `packages/sqlite` never imports an SDK or application crate — the
  existing `packages/*` rule. No `Network`, no `AssetId`, no table names.
- The package does not choose pragmas for you beyond safe defaults; WAL and
  busy timeout are `Options`, overridable, validated.
- One file, one pool, many owners: indexing, registry, and settings share
  the pool; each owns its tables and its steps. A `purge_network` still
  never touches application tables — that rule now has a structural echo:
  the owners are separate step-sets over one primitive.

## 4. Work order

1. **`packages/sqlite`.** Move `Pool`, the lease, `run`, `write`/`read`,
   the migration runner, pragmas, and error classification out of
   `indexing::sqlite`. Standalone tests: bound enforcement, lease return,
   migration apply and unknown-version refusal, busy classification.
2. **Rewire `indexing::sqlite`.** `Database` over the package; domain
   modules keep their SQL. Both contract suites pass unmodified — the
   parity proof.
3. **Rewire the server.** One pool in `storage.rs`; registry and settings
   as their own step-sets; `Store(indexing::sqlite::Database)` deleted;
   `Database::from_pool` at composition.
4. **Docs.** `packages/sqlite/README.md` with a usage example written for
   an unrelated project (a todo app, not a chain); `ARCHITECTURE.md` table;
   the `packages/*` ownership row.

## 5. Verification

The eight gates from `AGENTS.md`, plus the Linter MCP `check`.

- **Parity.** Every indexing repository contract suite passes against
  SQLite after the rewire, with no test edits — the extraction moved
  mechanics, not behavior.
- **Migration safety.** A file at version N opens at step N+1; a file at an
  unknown version is refused; a failed step rolls the version back with the
  rows.
- **One file, one pool.** The server test opens one file, runs indexing,
  registry, and settings on it, and restarts: everything survives, owners
  intact.
- **Independence.** A test crate (or the package's own example) uses
  `packages/sqlite` with no SDK dependency in its manifest — the
  "unrelated project" claim, compile-proved.

## 6. Out of scope

- **No async connection API.** Primitives stay sync; the one async helper
  is the off-executor runner.
- **No query building, no migrations DSL.** Steps are SQL text or
  closures; the package runs them, it does not invent them.
- **No behavior change in indexing.** The domain modules' SQL moves
  nowhere; only where their connections come from changes.
- **No merging of owners.** Registry, settings, and indexing keep separate
  step-sets over the shared pool; the package does not become a schema
  registry.
