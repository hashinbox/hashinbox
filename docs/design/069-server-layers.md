# 069 — Server layers: db, models, usecase, api, main

Status: done (all six steps). Deviations and answers: see "As built" at the end.

## Why

`apps/server/src/storage/` mixes four things:

1. the SQLite plumbing (pool, pragmas, `read`/`write` transactions);
2. schema execution (`migrate`, the embedded `schema/*.sql`) and the storage
   config;
3. the SDK's and the queue's store traits (`base::BlockStore`,
   `WalletIndexStore`, `InputStore`, `WalletStore`, `tasks::Store`,
   `tasks::Changes`) implemented over several models;
4. the server's own multi-model reads (`history`: balance and history pages;
   `wallets`: keyed and watched addresses).

Besides, some handlers assemble views from many models inline (the wallet
list, the chain view, the stats), and the trigger's evaluation is buried in
its worker where an endpoint cannot reuse it.

## Layers, bottom up

| # | Layer | Where | May depend on |
|---|---|---|---|
| 6 | packages | `packages/*` | third-party crates |
| 5 | sdk | `sdk/*` | packages |
| 4 | models | `apps/server/src/models/` | sdk, packages, `db` |
| 3 | usecase | `apps/server/src/usecase/` | models, sdk, packages, `db` |
| 2 | api | `apps/server/src/api/` | usecase, models, sdk, packages, `db` |
| 1 | main | `apps/server/src/main.rs` (+ `config.rs`, `ui.rs`) | everything |

`db` (`apps/server/src/db.rs`) sits beside the stack: it depends on
third-party crates only (diesel, r2d2, tokio) and anything may use it.

Rules:

- Nothing depends upward: no model uses a usecase, no usecase uses `api`,
  nothing but `main` uses `main`'s config.
- `api → models` and `api → usecase` are both allowed; a handler that needs one
  model calls it directly, one that combines several calls a usecase.
- A `usecase/*.rs` is business, not protocol: it combines models (and
  packages/sdk) into something more than one caller needs — a worker and an
  endpoint, two endpoints. Nothing HTTP or JSON-shaped goes there: no
  pagination, cursors, response bodies or status codes; those stay in `api`.
- Workers live in `usecase/workers/`, with the executor that runs them
  (`usecase/workers/executor.rs`); a worker may use models and the shared
  `usecase/*.rs`, never `api` and never another worker's internals. `main`
  builds the executor and adds the workers.

## Target layout

```
apps/server/src/
  main.rs          migrate / serve / --openapi; composes everything
  config.rs        the configuration document (db path included)
  ui.rs            the embedded page
  db.rs            Db: open(path), read, write, migrate(schemas); pragmas
  schema.rs        the embedded schema files (the one place that lists them)
  api/             handlers + AppState (from state.rs)
  models/          one file per table, unchanged
  usecase/
    sdk/           base's store traits on the models
      mod.rs       Stores { db } — the type the traits are on
      blocks.rs    BlockStore            (from storage/blocks.rs)
      indexes.rs   WalletIndexStore      (from storage/indexes.rs)
      inputs.rs    InputStore            (from storage/inputs.rs)
      wallets.rs   WalletStore           (from storage/wallets.rs, trait part)
    tasks.rs       the queue: tasks::Store and tasks::Changes on the task
                   model, with the change channel (from storage/tasks.rs)
    workers/       (from workers/)
      executor.rs  the job executor
      index.rs, expire.rs, prune.rs, broadcast.rs, trigger/{mod,triggers,actions}.rs
    trigger.rs     a trigger evaluated over its network: select, filter, when
                   (from workers/trigger/triggers.rs); the round worker uses it,
                   and later a preview endpoint can
    history.rs     an address's balance and mined transactions with their legs
                   (from storage/history.rs; the page cursor stays in api)
    wallets.rs     every wallet with its indexes and balances (from the api list)
    chain.rs       kept blocks with their transactions, and the pending ones
                   (from api/blocks.rs)
    stats.rs       the store's counts and hourly series (from api/stats.rs)
```

`storage/` disappears.

## db.rs

- `Db::open(path) -> Db` (absolute path, pool, pragmas), `Db::read(work)`,
  `Db::write(work)` (one `BEGIN IMMEDIATE` transaction), `Db::migrate(&[&str])`
  (executes the given schemas). No knowledge of models, schemas or the queue.
- The queue's change channel is not the database's: it moves to
  `usecase/tasks.rs`, whose queue type holds a `Db` and the
  `broadcast::Sender`; the executor and the workers take that queue.
- `main` runs `migrate`: `Db::open(config.db.path)?.migrate(schema::ALL)`.
  Tests do the same with `schema::ALL`. Whether `schema/*.sql` stays at the repo
  root or moves under `apps/server/` is open (see below).

## Second part: what moves out of api/workers into usecase/*.rs

| From | Into | Why |
|---|---|---|
| `workers::trigger::triggers` (`Trigger`, `Triggers`, `Context`) | `usecase/trigger.rs` | the round worker uses it now; a preview endpoint can later, without running a job |
|  | split at the action | `usecase/trigger.rs` ends with the decision (the wallets kept, whether *when* holds); invoking the action stays in the worker, so the usecase never depends on a worker |
| `api/wallets.rs` list (keyed + indexes + tags + balances) | `usecase/wallets.rs` → `list()` | four sources joined; the handler becomes read → map |
| `api/blocks.rs` block/tx/leg assembly | `usecase/chain.rs` | three models per block |
| `api/stats.rs` counts and series | `usecase/stats.rs` | ten models |
| `storage/history.rs` | `usecase/history.rs` | leg + transaction + block |
| job inputs `indexer::Target`, `trigger::Scope` | each worker's own file | used only by workers; the index worker queues the trigger round through its public input |

Stays where it is:

- `SendIntent`, `Payment`, `progress` stay with the broadcast worker, and the
  trigger actions (`Action`, `Actions`, `Transfer`, `SendInput`) stay with the
  trigger worker. Workers are in the usecase layer, so `api` using a worker's
  public types (the send endpoint's intent, the trigger API's action input) is
  `api → usecase`, which is allowed.
- Pagination, cursors and response shapes stay in `api`.

## Open questions

1. `schema/*.sql`: keep at the repo root (the SDK schema documents the SDK
   stores) or move to `apps/server/schema/` and embed from there?
2. The wallet list: drop `tags` from it (the UI already loads
   `GET /v1/wallets/tags`) when it moves to `usecase/wallets.rs`?
3. Name of the trait-carrying type in `usecase/sdk`: `Stores`?

## Order (one step at a time, each green: fmt, clippy, tests, UI checks)

1. `db.rs` + `schema.rs`; `storage::Storage` keeps working on top of `Db`.
2. `usecase/sdk/` (base's store traits), `usecase/tasks.rs` (the queue) and
   `usecase/history.rs`; `storage/` gone.
3. `usecase/workers/` (executor included); `workers/` gone.
4. `usecase/trigger.rs` out of the trigger worker.
5. `usecase/wallets.rs` (and the list shrink), `usecase/chain.rs`,
   `usecase/stats.rs`.
6. `AppState` into `api/`; a check (script or test) that no module imports
   upward.

## As built

- Open questions: `schema/*.sql` stays at the repo root (`schema.rs` embeds it
  from there); the wallet list keeps `tags` (JSON unchanged); the trait type is
  `Stores`.
- `Queue` keeps its `Db` private. A worker that writes other tables takes a
  `Db` beside the `Queue` (`Expire`, `Prune`, `Round`); `Broadcast` takes only
  a `Db`, `Index` only a `Queue`. `AppState` holds `db` and `queue`.
- `Storage::keyed()` is gone: the one-model read (`wallet::db::find_all`) is
  inline in the two handlers that need it and in `usecase::wallets::list`. The
  list now reads the keyed wallets in the same snapshot as the rest (it was a
  separate read before); the JSON is the same.
- `usecase::trigger`: `Context` has no `actions`; `Triggers::decide` takes the
  action as a closure over a `Decision` (action, network, `This`, the trigger
  still deciding, keyed addresses) and returns `None` where the check does not
  hold. The round worker builds the `Invocation` and runs `Actions` there.
- `usecase::stats` has its own plain structs; `api/stats.rs` maps them into
  the unchanged response shapes field by field.
- `usecase::chain` returns the model rows (`Block`, `Transaction`, `Legs`);
  `api/blocks.rs` turns them into its bodies with `TryFrom`.
- `workers/broadcast/` and `workers/indexer/` are flattened into
  `usecase/workers/{broadcast,expire,index}.rs`. `Expire` still uses the index
  job's public `Target`.
- `Db::migrate` takes `&[&'static str]` (the work runs on a blocking thread).
- The upward-import check is `scripts/server-layers.py` (run by hand; not
  wired into the tests).
