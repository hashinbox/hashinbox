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
- **Topics are the queue's lock.** A job locks a dot-separated topic
  (`n`, `n.w1`); one that is the other or a prefix of it conflicts. A job
  never starts beside, or ahead of, an earlier conflicting one.
- **Each rule in one place.** Starting a job and turning an outcome into the
  next state are pure functions.

## Design

### `Job`

```rust
pub struct Job {
    pub id: Id,                 // UUIDv7: orders jobs
    pub topic: Topic,           // what it locks while unfinished
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
    /// A pending job of `kind` on `topic`, runnable now.
    pub fn new(topic: Topic, kind: impl Into<String>, input: Value) -> Self;
    /// Running with its attempt counted, or failed `expired`.
    pub fn start(self, now: u64) -> Self;
    /// The job after a worker's answer (see the table).
    pub fn settle(self, ack: Ack, retry: &Retry, now: u64) -> Self;
}

/// What a job locks: segments, broadest first. What they mean is the
/// caller's (the server: `chain:network.address` for a send, `chain:network`
/// for a whole network).
pub struct Topic(String);

impl Topic {
    pub fn new<S: AsRef<str>>(segments: impl IntoIterator<Item = S>) -> Option<Self>;
    pub fn parse(topic: &str) -> Option<Self>;
    /// One covers the other: matches a prefix of it at a dot, `*` taking
    /// one or more segments (`*.w1` covers `n.w1` and `n.x.w1`).
    pub fn conflicts(&self, other: &Self) -> bool;
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
    /// What may start now, oldest first: pending jobs due at `now` with no
    /// running or earlier unfinished job of a conflicting topic.
    fn next(&self, now: u64) -> StoreFuture<'_, Vec<Job>>;
}

/// When to ask `next` again.
pub trait Changes: Send + Sync {
    /// Fires when `next` may answer differently: a job enqueued or updated, or time passed.
    fn changes(&self) -> broadcast::Receiver<()>;
}
```

- `next` reads the unfinished jobs (by id) and weighs topics in Rust (`*`
  is no SQL prefix); no two jobs it returns conflict. A job waiting on its backoff still holds
  its topic. Jobs `b n.w1, t n, b n.w1, b n.w1, b n.w2` start as
  `[n.w1] [n] [n.w1, n.w2] [n.w1]`.
- **Recovery is the store's.** A job left `Running` by a dead process would
  block its topic; a store sets its running jobs back to `Pending` when it
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
how a worker makes its jobs (topic, deadline) is its own code.

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
    /// The job: id, topic, attempts, reason, deadline. Read only.
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
struct Queued { inner: Arc<dyn rpc::Client>, store: Arc<dyn Store>, topic: Topic }

impl rpc::Client for Queued {
    fn request(&self, r: Request) -> … { self.inner.request(r) }   // forwarded
    fn batch(&self, b: Batch) -> …     { self.inner.batch(b) }     // forwarded
    fn broadcast(&self, r: Request) -> … {
        // accepted = saved; the worker sends it later
        self.store.enqueue(Job::new(self.topic.clone(), Broadcast::KIND, json!(r))).await?;
        Ok(CallResult::accepted())
    }
}

let rpc = Arc::new(rpc::Client::from(endpoints));
let client = Queued { inner: rpc.clone(), store: store.clone(), topic: Topic::new([network])? };
workers.add(Broadcast { rpc }, retry);
chain = Chain::new(client); // SDK calls broadcast, the store gets a job
```

- The SDK is unchanged: it calls `broadcast` as before.
- The job runs eventually, one at a time per topic.
- `State` stays `()` here; a worker that must not redo a step saves it.


A store sharing a database with the caller may let the caller save a state
inside its own transaction (066's `built` state). That is the caller's store,
not the queue's API.

### Rules

`Job::start` and `Job::settle`, the whole state machine; `Retry` is at most
`attempts` executions, waiting `backoff × attempts`.

| Ack | Next |
| --- | --- |
| `Done` | `Done` |
| `Fail(r)` | `Failed`, `reason = r` |
| `Retry(r)`, attempts left | `Pending`, `run_at = now + backoff × attempts`, `reason = r` |
| `Retry(r)`, none left | `Failed`, `reason = "attempts-exhausted: r"` |

### Executor

The server's (`workers/executor.rs`), not the package's: a worker and
`Retry` per kind; recovers on start; starts what `next` returns while there
is room; runs each job on its own task (unknown kind fails `unknown-kind`, a
panic `panicked`); stores the settled job, retried until it lands; waits on
`Ticking` over the store's changes and on executions ending.

### Behaviour kept

- Conflicting topics run one at a time, oldest first; a waiting job holds its topic.
- A worker must tolerate a rerun (resume from its saved state).
- Finished jobs stay in the store as their record.

## Not in this design

- Results beyond `Done` (a job's output is its saved state).
- Idempotency keys, cancelling, several processes on one store (a lease).
- A `Handle` to await a job: a caller polls `get` on `changes`.

## Server

- `schema/queue.sql`: `q` gains `deadline`, `attempts`, `reason`, `state`;
  `payload` becomes `input`, any JSON; `lane` becomes `topic`; index on unfinished jobs. Fresh database.
- `models/task.rs`: the columns; `db::next(now)`; `db::recover` on open.
- `storage/tasks.rs`: `Store` and `Changes`; the runner gets `Ticking` over the changes.
- `workers/send.rs`: a `Worker`; signed transaction is its `State`.
- `api/jobs.rs`: `attempts` and `reason` from the job's fields.

## Plan

1. `Job` typed fields; `Store { enqueue, update, get, next(now) }`; `Changes`; `Ticking`.
2. `start`, `settle`, `Retry`.
3. `Worker` (`KIND`, `Input`, `State`, `run`), `Task`, `Ack`; the server's executor.
4. Server: schema, task model with recovery on open, store, send worker,
   jobs API.
5. Tests: `start` and `settle` tables; topic order; conflicting topics one at a time;
   expiry; retry backoff and exhaustion; unknown kind; panic; recovery on
   open; a `Task::save` survives a rerun; settling survives a store failure;
   `next` never overtakes a conflicting topic; `Ticking` fires with no write.
