# 067: Task queue

Status: draft.

`packages/tasks` runs durable jobs: one at a time per lane, oldest first,
retried with backoff, resumed after a crash. 066 builds on it (a broadcast
job per network lane; trigger rounds per network). Its runner works, but its
rules are hard to see: one function does six things, queue state lives in the
caller's JSON, and executors write to the store behind the runner's back.
This design keeps the behaviour and makes each rule have one place.

## What is wrong today

1. **`Runner::fill` does six things** under a name that says none: it picks
   lane heads, skips running ones, fails expired ones, computes the next wake,
   checks free slots, and validates, counts and starts a job.
2. **Queue state hides in the payload.** `attempts`, `reason` and `deadline`
   are runner-owned keys inside the executor's own `payload`; a payload that
   is not a JSON object fails as `payload-not-object`. `Job` types `state` and
   `run_at` but not these.
3. **Two writers to one job.** `Executor::execute(job, store)` gets the whole
   store; an executor saves progress with `store.set` on its running job, so
   the runner re-reads the job after it returns to pick those writes up.
4. **A lane can stay blocked.** When storing an outcome fails five times
   (`SETTLE_TRIES`), the job stays `Running` and its lane waits for a restart.
5. **Every wake reads every unfinished job.** `store.next(u64::MAX)` (its
   `now` is never anything else) loads the whole backlog; lane heads are then
   found in memory.
6. **The retry rule is split.** Attempts are counted when a job starts
   (`fill`), backoff and exhaustion decided when it settles
   (`Execution::settle`).
7. **Small confusions.** `settled()` also builds a pending retry; `run` mixes
   crash recovery with the loop; `PAUSE` is both the store-failure wait and
   the unit of the idle wait (`PAUSE * 60`); `now()` is a public helper of the
   runner module.

## Principles

- **Primitives, not a framework.** `tasks` gives the job type, the traits and
  the rules as plain functions. The caller (the server) composes them and may
  implement any trait itself.
- **Small traits.** Each trait has the fewest methods its user needs; cleverness
  (timers, recovery, transactions) lives in implementations and decorators.
- **The queue owns queue fields; the worker owns its input and state.**
  Attempts, reason, deadline, run time and status are typed fields of `Job`;
  input and state are the worker's, any JSON.
- **A lane is one job at a time.** It is the queue's lock; more parallelism
  means more lanes.
- **Each rule in one place.** Starting a job and turning an outcome into the
  next state are pure functions.

## Design

### `Job`

```rust
pub struct Job {
    pub id: Id,                 // UUIDv7: orders a lane
    pub lane: String,           // one running job per lane, oldest first
    pub kind: String,           // which worker runs it
    pub status: Status,         // Pending | Running | Done | Failed
    pub run_at: u64,            // Unix ms; not started before
    pub deadline: Option<u64>,  // Unix ms; not started after
    pub attempts: u32,          // executions started
    pub reason: Option<String>, // why it failed, or why it last retried
    pub input: serde_json::Value,         // the worker's: what to do, never changed
    pub state: Option<serde_json::Value>, // the worker's: done so far, saved while running
}

impl Job {
    /// A pending job of `kind` on `lane`, runnable now.
    pub fn new(lane: impl Into<String>, kind: impl Into<String>, input: Value) -> Self;
    /// The same, not started after `deadline`.
    pub fn with_deadline(self, deadline: u64) -> Self;
}
```

### `Store` and `Changes`

```rust
pub trait Store: Send + Sync {
    /// Adds a new job.
    fn enqueue(&self, job: Job) -> StoreFuture<'_, ()>;
    /// Replaces a stored job (started, settled, progress saved).
    fn update(&self, job: Job) -> StoreFuture<'_, ()>;
    /// The job `id`.
    fn get<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Job>>;
    /// The oldest pending job of every lane with no running job, when due
    /// (`run_at <= now`): what may start now, by lane.
    fn next(&self, now: u64) -> StoreFuture<'_, BTreeMap<String, Job>>;
}

/// When to ask `next` again.
pub trait Changes: Send + Sync {
    /// Fires when `next` may answer differently: a job enqueued or updated, or time passed.
    fn changes(&self) -> broadcast::Receiver<()>;
}
```

- `next` is one indexed query (a lane's head is its smallest unfinished id);
  a head waiting on its backoff is not returned and still holds its lane.
- **Recovery is the store's.** A job left `Running` by a dead process would
  block its lane; a store sets its running jobs back to `Pending` when it
  opens (single process).
- **Time is the signal's.** A retry becomes due with no write, so `changes`
  also fires as time passes. A decorator adds that to any store:

```rust
/// `inner`'s changes, plus one every `period`.
pub struct Ticking<C> { inner: C, period: Duration }

impl<C: Changes> Changes for Ticking<C> { /* inner's, plus the timer */ }
```

### `Worker` and `Task`

A worker does one kind of job. The trait is only what the executor needs;
how a worker makes its jobs (lane, deadline) is its own code.

A job carries two things of its worker's:

- **input**: what to do, set when enqueued, never changed;
- **state**: what the worker has done so far, saved while running, read on a
  rerun. Absent until first saved; a worker with nothing to keep uses `()`.

```rust
pub trait Worker: Send + Sync + 'static {
    /// The kind name stored on its jobs.
    const KIND: &'static str;
    /// What to do.
    type Input: Serialize + DeserializeOwned + Send;
    /// What is done so far; `()` when a rerun needs nothing.
    type State: Serialize + DeserializeOwned + Send;

    /// Runs one job and answers it.
    fn run(&self, task: Task<Self::Input, Self::State>) -> impl Future<Output = Ack> + Send;
}

/// One running job, as its worker sees it.
pub struct Task<I, S> { job: Job, input: I, state: Option<S>, store: Arc<dyn Store> }

impl<I, S: Serialize> Task<I, S> {
    /// The job: id, lane, attempts, reason, deadline. Read only.
    pub fn job(&self) -> &Job;
    pub fn input(&self) -> &I;
    /// The last saved state; `None` on a first run.
    pub fn state(&self) -> Option<&S>;
    /// Stores `state` now; a rerun starts from it.
    pub async fn save(&mut self, state: S) -> Result<(), StoreError>;
    /// Done.
    pub fn ack(self) -> Ack;
    /// Retried while attempts are left.
    pub fn nack(self, reason: impl Into<String>) -> Ack;
    /// Failed, not retried.
    pub fn fail(self, reason: impl Into<String>) -> Ack;
}

pub enum Ack { Done, Fail(String), Retry(String) }
```

- Answering consumes the task: nothing is saved after it.
- Queue fields and input are read only to a worker; it changes only its state.
- `Ack` is still returned, so the executor settles a panic or a missing
  answer too.

Use:

```rust
/// Sends a queued broadcast through the network's real rpc.
struct Broadcast { rpc: Arc<dyn rpc::Client> }

impl Worker for Broadcast {
    const KIND: &'static str = "broadcast";
    type Input = rpc::Request; // the signed envelope, exactly as the SDK sent it
    type State = ();            // the bytes are the input: a rerun resends them

    async fn run(&self, task: Task<rpc::Request, ()>) -> Ack {
        match self.rpc.broadcast(task.input().clone()).await {
            Ok(_) => task.ack(),
            Err(e) if e.is_definite() => task.fail(e),
            Err(e) => task.nack(e),
        }
    }
}
```

Callers never see the queue. The server builds the network's rpc from its
endpoints and wraps it in a client that queues; the SDK gets that client:

```rust
/// The network's rpc, except `broadcast`: it enqueues and returns.
struct Queued { inner: Arc<dyn rpc::Client>, store: Arc<dyn Store>, lane: String }

impl rpc::Client for Queued {
    fn request(&self, r: Request) -> … { self.inner.request(r) }   // forwarded
    fn batch(&self, b: Batch) -> …     { self.inner.batch(b) }     // forwarded
    fn broadcast(&self, r: Request) -> … {
        // accepted = saved; the worker sends it later
        self.store.enqueue(Job::new(&self.lane, Broadcast::KIND, json!(r))).await?;
        Ok(CallResult::accepted())
    }
}

let rpc = Arc::new(rpc::Client::from(endpoints));
let client = Queued { inner: rpc.clone(), store: store.clone(), lane: format!("broadcast:{network}") };
workers.add(Broadcast { rpc }, retry);
chain = Chain::new(client); // SDK calls broadcast, the store gets a job
```

- The SDK is unchanged: it calls `broadcast` as before.
- The job runs eventually, in its network's lane, one at a time.
- `State` stays `()` here; a worker that must not redo a step saves it.


A store sharing a database with the caller may let the caller save a state
inside its own transaction (066's `built` state). That is the caller's store,
not the queue's API.

### Rules

Two pure functions, the whole state machine:

```rust
/// `job` taken from `next` at `now`: `Running` with its attempt counted, or
/// `Failed("expired")` past its deadline.
pub fn start(job: Job, now: u64) -> Job;

/// The job after `ack`.
pub fn settle(job: Job, ack: Ack, policy: &Retry, now: u64) -> Job;

/// How retries go: at most `attempts`, waiting `backoff × attempts`.
pub struct Retry { pub attempts: NonZeroU32, pub backoff: Duration }
```

| Ack | Next |
| --- | --- |
| `Done` | `Done` |
| `Fail(r)` | `Failed`, `reason = r` |
| `Retry(r)`, attempts left | `Pending`, `run_at = now + backoff × attempts`, `reason = r` |
| `Retry(r)`, none left | `Failed`, `reason = "attempts-exhausted: r"` |

### `Executor`

Runs started jobs; how (tasks, threads, a pool, in line) is its business.

```rust
pub trait Executor: Send + Sync {
    /// Runs `job` (already `Running` and stored) to its end and stores the
    /// settled job.
    fn execute(&self, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}
```

`tasks` ships one, `Workers`: a worker per kind with its `Retry` (`add(worker, retry)`), `settle`, a
store failure on settling retried until stored. Unknown kind fails
`unknown-kind`; a panic fails `panicked`. The server may write its own.

### The loop

Twenty lines, shipped as a function; a caller may write its own:

```rust
/// Starts due jobs on `executor`, at most `concurrency` at once, until dropped.
pub async fn run(store: &dyn Store, changes: &dyn Changes, executor: &dyn Executor, concurrency: NonZeroUsize) -> Result<(), StoreError> {
    let mut signal = changes.changes();
    let mut running = JoinSet::new();
    loop {
        for job in store.next(now()).await?.into_values().take(free(&running)) {
            let job = start(job, now());
            store.update(job.clone()).await?;
            if job.status == Status::Running { running.spawn(executor.execute(job)); }
        }
        select! { _ = signal.recv() => {}, _ = running.join_next() => {} }
    }
}
```

### Behaviour kept

- One running job per lane, oldest first; a waiting head holds its lane.
- A worker must tolerate a rerun (resume from its saved state).
- Finished jobs stay in the store as their record.

## Not in this design

- Results beyond `Done` (a job's output is its saved state).
- Idempotency keys, cancelling, several processes on one store (a lease).
- A `Handle` to await a job: a caller polls `get` on `changes`.

## Server

- `schema/queue.sql`: `q` gains `deadline`, `attempts`, `reason`, `state`;
  `payload` becomes `input`, any JSON; index on unfinished jobs by `(lane, id)`. Fresh database.
- `models/task.rs`: the columns; `db::next(now)`; `db::recover` on open.
- `storage/tasks.rs`: `Store` and `Changes`; the runner gets `Ticking` over the changes.
- `workers/send.rs`: a `Worker`; signed transaction is its `State`.
- `api/jobs.rs`: `attempts` and `reason` from the job's fields.

## Plan

1. `Job` typed fields; `Store { enqueue, update, get, next(now) }`; `Changes`; `Ticking`.
2. `start`, `settle`, `Retry`.
3. `Worker` (`KIND`, `Input`, `State`, `run`), `Task`, `Ack`; `Executor` and `Workers`; `run`.
4. Server: schema, task model with recovery on open, store, send worker,
   jobs API.
5. Tests: `start` and `settle` tables; lane order; one running per lane;
   expiry; retry backoff and exhaustion; unknown kind; panic; recovery on
   open; a `Task::save` survives a rerun; settling survives a store failure;
   `next` returns one due head per free lane; `Ticking` fires with no write.
