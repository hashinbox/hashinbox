# 076: Jobs: worker policy, executor and store in `tasks`

Status: implemented, with the deviations listed at the end. Changes
`packages/tasks` (approved) and the server's workers.

067 gave the queue one lock rule (topics) and one state machine. Everything
else a job needs (when it runs, what happens on a held lock, how many run,
how long a run may take) is either missing or hand-rolled per worker in the
server. This design moves those rules into `tasks`, next to the lock, and
makes every decision the queue takes visible.

## Review: what we have

### `packages/tasks`

- `Job`: `id` (UUIDv7, orders jobs), `topic`, `kind`, `status`, `run_at`,
  `deadline` (not started after), `attempts`, `reason`, `input`, `state`.
- `Topic`: dotted segments; two conflict when one covers the other.
- `Jobs::blocking`: a job waits for a conflicting unfinished job that is
  running or has a lower id. Pure, one place.
- `Job::start` / `Job::settle(Ack, Retry)`: the state machine. `Again(at)`
  and `AgainBecause(at, why)` make a job a poller.
- `Store`: `enqueue`, `update`, `get`, `next(now)`. `Changes`: a signal to
  ask `next` again. `Ticking`: `Changes` plus a period.
- `Worker`: `KIND`, `Input`, `State`, `run(Task) -> Ack`. Nothing else.

### Server (`apps/server/src/usecase`)

- `workers/executor.rs`: the loop, a kind → (run, `Retry`) map, a global
  `CONCURRENCY = 4`, panics caught, settlement retried until stored. Nothing
  in it is server specific.
- `tasks.rs` (`Queue`): `Store` + `Changes` on SQLite, `announce()` by hand
  after each write, `enqueue_alone`, `recover`.
- Recurring jobs (`index`, `expire`, `prune`): each worker has its own
  `start()` that builds a job with a **fixed id**, enqueues it or revives a
  finished one, and each run ends with `task.again(later)`.
- One-shot jobs: `triggers` (queued by `index` through `enqueue_alone`),
  `broadcast` (queued by a round and by `POST …/transactions`).

### What is wrong

1. **A held lock has one answer: wait, behind everyone older.** A round
   queued while a broadcast is stuck waits, then runs on what it saw being
   older than the next broadcast (075 F1). `enqueue_alone` patches it
   outside the queue, by kind + topic, and its answer is thrown away
   (`let _ =`).
2. **Correctness rests on id order.** "Older" is "lower id". The id is
   made when the job is built (`Job::new`), not when stored; it follows the
   wall clock; fixed ids (`index.eth.sepolia`) sort after every UUIDv7. None
   of it is checked.
3. **Recurring jobs are hand-rolled three times.** `start()` with a fixed id,
   enqueue-or-revive, `again(later)`: the same 20 lines in `index`,
   `expire`, `prune`.
4. **No run timeout.** `deadline` only stops a job from *starting*. A run
   that hangs holds its topic forever: a stuck broadcast locks its network
   and every round behind it.
5. **Concurrency is one global number.** Ten indexing networks can take all
   four slots; a payout waits for an indexer step.
6. **Retry is configured beside the worker** (`Executor::add(worker,
   retry)`), not by it.
7. **Two writers to the queue table.** A round inserts its broadcast rows
   inside its own transaction (`task::db::insert`) and calls
   `queue.announce()`; 067 named this as a problem and it came back.
8. **The executor lives in the server** although it is generic.
9. **Silent.** The server's `Logger` has no sink; the executor logs nothing;
   no log carries a job id. The only trace is the `task` row.
10. **`next` loads every unfinished job** on every wake (067 issue 5, open).

## Principles

- **The worker declares, the queue decides.** A worker says how its jobs
  run (schedule, lock policy, concurrency, timeout, retry); the queue
  applies it. No worker re-implements a rule.
- **Every decision has an answer.** Enqueue, skip, start, retry, time out,
  fail: each is a typed outcome and a log event carrying the job id.
- **Order is the store's, not the clock's.** A store assigns a sequence
  number when a job enters the line.
- **Rules stay pure.** Admission, startability and settlement are functions
  over `Job`/`Jobs`, as in 067; stores and the executor only apply them.

## Design

### `Policy`: what a worker declares

```rust
pub trait Worker: Send + Sync + 'static {
    const KIND: &'static str;
    type Input: Serialize + DeserializeOwned + Send;
    type State: Serialize + DeserializeOwned + Send;

    /// How its jobs run. Read once, when the executor adds the worker.
    fn policy(&self) -> Policy;

    fn run(&self, task: Task<Self::Input, Self::State>) -> impl Future<Output = Ack> + Send;
}

pub struct Policy {
    pub schedule: Schedule,
    pub on_lock: OnLock,
    /// Runs of this kind at once (the executor also has a global cap).
    pub concurrency: NonZeroUsize,
    /// Longest a run may take; past it the run is dropped and retried.
    pub timeout: Option<Duration>,
    pub retry: Retry,
}

pub enum Schedule {
    /// Queued by someone (an API call, another job).
    Manual,
    /// Re-armed `period` after a run that ended `Done`, unless the run
    /// answered `again(at)`. Who creates an instance enqueues it once,
    /// with a key; a repeat is `Duplicate`.
    Every(Duration),
}

pub enum OnLock {
    /// Wait in line behind earlier conflicting jobs; fail `lock-timeout`
    /// (with the holder's id) after `max`.
    Wait { max: Option<Duration> },
    /// Do not run while any conflicting job is unfinished: refused at
    /// enqueue, or finished `skipped` at start, with the holder's id.
    Skip,
}
```

Our workers:

| kind | schedule | on_lock | concurrency | timeout |
|---|---|---|---|---|
| `index` | `Every(interval)`, one per network | `Wait` | 4 | 5 min |
| `expire` | `Every(PERIOD)`, one per network | `Wait` | 1 | 60 s |
| `prune` | `Every(period)`, one | `Wait` | 1 | 60 s |
| `triggers` | `Manual` | `Skip` | 2 | 60 s |
| `broadcast` | `Manual` | `Wait { max: None }` | 4 | 60 s |

`index` keeps answering `again(now)` while behind; at the tip it answers
`ack()` and the schedule re-arms it.

### `Job`: two new fields, one new status

- `key: Option<String>`: unique among **unfinished** jobs of a kind. A
  recurring job's key is its instance (`eth:sepolia`); a round's key is its
  network. Replaces fixed ids and `enqueue_alone`.
- `seq: u64`: the job's place in line, assigned by the store when the job
  is stored and again each time it goes back to pending for a new run
  (`Again`, schedule). Not for retries: a retry keeps its place. "Older"
  means lower `seq`. Ids go back to being identity only.
- `Status::Skipped`: finished without running, `reason` names the holder.

### Rules (pure, in `jobs.rs`)

- `Jobs::admit(&job, on_lock) -> Admission`:
  - `Duplicate(id)`: an unfinished job of the same kind and key exists.
  - `Skipped(holder)`: `Skip` and a conflicting job is unfinished.
  - `Queued`.
- `Jobs::startable(now) -> Vec<Start>`, oldest first:
  - `Run(job)`: due, nothing blocks it, under its kind's concurrency.
  - `Skip(job, holder)`: `Skip` and any conflicting job is unfinished,
    older **or newer** (a newer one queued since, e.g. a send from the API,
    waits behind it otherwise).
  - `Expire(job, holder)`: `Wait { max }` passed while blocked.
- `Job::start` / `Job::settle`: as now, plus `Timeout` (an `Ack` the
  executor makes) settling as a retry with reason `timeout`, and re-arm
  for `Every` on `Done`.

### `Store`: what a durable queue implements

```rust
pub trait Store: Send + Sync {
    /// Admits `job` (`Jobs::admit` over the unfinished jobs it conflicts
    /// with or shares a key with) and stores it if queued, atomically.
    fn enqueue(&self, job: Job, on_lock: OnLock) -> StoreFuture<'_, Admission>;
    /// Replaces a stored job whole; assigns a new `seq` when it went back to
    /// pending for a new run.
    fn update(&self, job: Job) -> StoreFuture<'_, ()>;
    fn get<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Job>>;
    /// The unfinished jobs, by `seq` (the input of `Jobs::startable`).
    fn unfinished(&self) -> StoreFuture<'_, Jobs>;
    /// Running jobs back to pending; called once by the executor on start.
    fn recover(&self) -> StoreFuture<'_, usize>;
}
```

- `tasks` ships `MemoryStore` (tests, examples) and a **contract suite**
  (`tasks::testing::store_contract`, feature `testing`) every store runs:
  atomic admit, key uniqueness, `seq` order, recover, settle round-trips.
  The server's SQLite store runs it.
- `next` leaves the trait: `startable` is a rule, so the executor computes it
  from `unfinished()`. A store may still filter in SQL later (issue 10)
  behind the same contract.

### `Notifying<S>`: the change signal as a decorator

```rust
/// `inner`, signalling after every write that landed.
pub struct Notifying<S> { inner: S, sender: broadcast::Sender<()> }
impl<S: Store> Store for Notifying<S> { /* delegate, then send */ }
impl<S> Changes for Notifying<S> { /* subscribe */ }
impl<S> Notifying<S> { pub fn notify(&self) { /* for writes outside the store */ } }
```

- Stores stop announcing; `Queue::announce` goes.
- `Ticking` stays: it wraps `Changes`, so due retries and schedules fire
  with no write.
- `notify()` is for the one remaining outside write (below). A missed
  notify costs one tick, never correctness.

### `Executor` in `tasks` (feature `runner`)

```rust
let executor = Executor::new(store, Limits { concurrency: 8, tick: 1s }, logger)
    .add(index)
    .add(round)
    .add(broadcast);
executor.run().await;
```

- On start: `recover()`. It creates no jobs: the server enqueues one job
  per recurring instance at boot (`main.rs`, one `index` and `expire` per
  network, one `prune`), and the API when a network is added. By key, so a
  repeat is `Duplicate`; a failed one (retries exhausted) is replaced.
- Loop: `unfinished()` → `startable(now)` → apply `Skip`/`Expire` →
  start `Run`s up to the global and per-kind caps → wait for a change, a
  tick or a finished run.
- Each run in its own task (a panic is `Fail("panicked")`), wrapped in
  `tokio::time::timeout(policy.timeout)`; settlement retried until stored,
  as now.
- Unknown kinds fail `unknown-kind`, as now.

### Logs (every event carries `job`, `kind`, `topic`, `key`)

| event | when | extra fields |
|---|---|---|
| `job.queued` | admitted | `seq` |
| `job.duplicate` | admit found the same key | `existing` |
| `job.skipped` | `Skip` at enqueue or start | `holder` |
| `job.started` | run begins | `attempt` |
| `job.done` | `Done` | `ms` |
| `job.again` | re-armed | `at`, `reason` |
| `job.retry` | `Retry` | `at`, `reason` |
| `job.timeout` | run past `timeout` | `ms` |
| `job.lock_timeout` | `Wait { max }` passed | `holder` |
| `job.failed` | `Fail` / exhausted / panicked | `reason` |

`Task::logger()` hands the worker a logger already carrying those fields,
so a worker's own events (`trigger.refused`, `trigger.acted`) carry the job
id too. The server wires a real sink (stderr) in `main.rs`.

`tasks` gains a dependency on `packages/logging` (packages → packages).

### The round, after

- `index` at the tip: `queue.enqueue(round, Skip)`; the `Admission` is
  logged by the queue. No `enqueue_alone`, no `let _`.
- A round runs only when no job of its network is unfinished: never beside
  or behind a broadcast it cannot see. A newer send queued meanwhile makes
  it skip at start.
- Its decisions and their broadcast jobs stay one transaction (075 F2): the
  round writes the rows through the server's own `task` model and calls
  `notify()`. This is the one write outside `Store`; it is allowed because
  the round's `Skip` policy already guarantees nothing conflicting is
  unfinished, and admission is trivially `Queued`.
  (Alternative, not chosen: save decisions as the round's `state`, then
  enqueue each broadcast with a deterministic key; a rerun re-enqueues and
  gets `Duplicate`. No outside write, but trigger runs and jobs are no
  longer one transaction.)

## What goes

- `enqueue_alone`, `insert_alone`, `Queue::announce`.
- `start()` in `index`, `expire`, `prune`; fixed job ids (`index::job_id`).
- `apps/server/src/usecase/workers/executor.rs` (moves to `tasks`).
- `Executor::add(worker, retry)`'s `retry` argument.
- `Store::next`.

## Schema (server `task` table)

- `key TEXT NULL`, partial unique index on `(kind, key)` where
  `status IN (0, 1)`.
- `seq INTEGER NOT NULL`, index; assigned `MAX(seq) + 1` in the write.
- `status` 4 = skipped.
- No migration: fresh schema, as elsewhere.

## Steps

1. `tasks`: `Policy`, `OnLock`, `Schedule`; `key`, `seq`,
   `Skipped`; `Jobs::admit`, `startable` with `Start`; settle for
   `Timeout`/`Every`. Unit tables for each rule.
2. `tasks`: `Store` reshaped, `MemoryStore`, contract suite, `Notifying`.
3. `tasks`: `Executor` moved in, per-kind caps, timeout, logs.
4. Server: SQLite store on the new trait (runs the contract), `Notifying`
   around it, workers declare `policy()`, `start()`s replaced by keyed enqueues in
   `main.rs`,
   index enqueues rounds with `Skip`, stderr sink in `main.rs`.
5. Tests: the 075 scenarios (`backlog`, `partial`, `queued`) unchanged and
   green; new ones: a round skipped behind a stuck broadcast (log carries the
   holder id), a newer API send makes a pending round skip, a hung run times
   out and frees its topic, per-kind caps keep a broadcast slot free while
   ten networks index.

## Open

- Per-network `interval` for `index`: `Every(Duration)` is per kind, so
  `index` keeps answering `again(at)` with its network's interval.
- Timeout values per kind: 60 s is a placeholder until the timeouts plan
  (issues.md) sets per-method RPC budgets.
- `unfinished()` reads every unfinished job; fine at our sizes, revisit with
  issue 10.

## Deviations (as implemented)

- **`Skip` waits for earlier jobs; only a later one skips it.** Refusing a
  `Skip` job at enqueue while any conflicting job is unfinished would break
  the 075 `queued` scenario (one round waits behind a send stuck in retry).
  So a `Skip` job waits behind earlier conflicting jobs like `Wait`, and is
  finished `skipped` at start while a *later* conflicting job is unfinished:
  the case the design cares about (a newer API send it cannot see). It never
  runs on a view missing a job queued after it, and a round behind a stuck
  send decides once the send ends.
- **Admission is the key alone.** With the rule above nothing is refused at
  enqueue by its topic, so `Admission` is `Queued(seq)` or `Duplicate(id)`
  and `Store::enqueue` takes no `on_lock`. There is no `Jobs::admit`: each
  store checks the key in its own write (the SQLite store also has a partial
  unique index on `(kind, key)`), and the contract suite pins it.
- **A new place in line is `seq` 0.** `settle` sets `seq` to 0 when a job
  goes back to pending for a new run (`Again`, `AgainBecause`, an `Every`
  re-arm); a store gives a job with `seq` 0 the next place on `enqueue` or
  `update`. A retry keeps its `seq`.
- **No `Timeout` ack.** The executor drops a run past its timeout, logs
  `job.timeout` and settles it as `Ack::Retry("timeout")`.
- **`index` is `Manual`.** It never answers `Done`; it answers `again(at)` at
  its network's interval (the per-network interval in Open), so an `Every`
  period would never apply.
- **`Wait { max }`** counts from the job's `run_at` (when it fell due).
- **Admissions are logged by `Notifying`** (it holds a logger), so every
  enqueue, from any caller, logs `job.queued` or `job.duplicate`.
- **The round's outside write** is allowed because its broadcast jobs have
  no key: admission is trivially `Queued`.
- **Tests.** The two round scenarios are one server case (`skipped`): a
  round waiting behind a stuck send is skipped once a newer send is queued,
  its reason and its `job.skipped` log naming that send. The hung-run
  timeout and the per-kind cap are executor tests in `tasks` over
  `MemoryStore`, not server cases.
- **Prune also deletes skipped rounds**; they are as much noise as
  succeeded ones.
- **The stderr sink prints `info` and above**: skips, retries, timeouts,
  lock timeouts and failures. `job.queued`, `job.duplicate`, `job.started`,
  `job.again` and `job.done` are `debug`.
- **No job is queued when a network is added at runtime**: networks are
  built at boot, and boot enqueues every instance.
- **Recurring jobs retry for ever.** With the default retry (three
  attempts), three timeouts or failed writes in a row failed `index`,
  `expire` or `prune` for good, and nothing re-armed it until a restart.
  They now retry with `FOREVER` (`workers/mod.rs`): attempts never run out,
  each retry waits a second longer than the last (`backoff × attempts`), and
  a run that ends `again` or `Done` starts the count over. `index` also has
  its own timeout, 5 min, above one slow Ethereum step (~3 min, issues.md
  T1), so a run always makes progress before it can be dropped. The test is
  `index::tests::an_index_job_timing_out_again_and_again_stays_pending`: 100
  timeouts in a row leave the job pending with a later `run_at`.
- **The `partial` case created its two triggers in one batch.** It created
  `a` and `b` in two API calls. A round could run between them: it decided
  `a` alone and kept its run and send, and the quarter that send held left
  the treasury below 0.9, so neither trigger decided again and no round ever
  failed at `b` (seen as 1 in ~25 loaded runs; confirmed with `a`'s run
  present at the failure). The queue never promised a round would not run
  between two API calls; the case now creates both in one `POST
  /v1/triggers`, which is stored as one write, so a round sees both or
  neither.
- **Where the removed server queue tests live.** The SQLite store's own
  queue tests became `the_sqlite_store_keeps_the_contract`, which runs
  `tasks::testing::store_contract` (as does `MemoryStore`):

  | removed test | now tested by |
  |---|---|
  | `update_replaces_the_job_whole` | `store_contract`, "an update replaces the job whole" |
  | `next_never_starts_a_conflicting_topic_or_overtakes` | `store_contract`, "what may start from the line" (the same steps, through the store's `unfinished` and `Jobs::startable`); `jobs::tests::nothing_starts_beside_or_ahead_of_a_conflicting_job` |
  | `a_wildcard_locks_its_wallet_on_every_network` | `store_contract`, "a wildcard locks its wallet on every network" |
  | `enqueue_alone_waits_for_the_unfinished_one` | `store_contract`, "a key is unique among the unfinished jobs of a kind" (pending, running, another kind, no key, after done, failed or skipped, concurrent) |
  | `recover_returns_running_jobs_to_pending` | `store_contract`, "running jobs go back to pending" |
  | `the_queue_retries_saves_fails_unknown_kinds_and_expires` | `executor::tests::it_retries_saves_fails_unknown_kinds_and_expires` (over `MemoryStore`) |
