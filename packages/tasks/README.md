# tasks

Own background Tokio tasks, refuse new work after closing, and wait for every
accepted task while retaining the first observed task failure.

## Usage

```rust
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let tasks = tasks::Tasks::new();
tasks.spawn(async { /* background work */ })?;
tasks.close();
tasks.drain().await?;
# Ok(())
# }
```

`spawn` requires a running Tokio runtime. Refused work is never spawned.
Completed tasks are reaped on registration; failures remain available to `drain`.
Dropping the owner aborts tasks it still owns. Drive shutdown with one drain:
cancelling it aborts the tasks transferred to that drain, and concurrent drains
do not wait for each other's work. Task results are `()`; application errors
remain the task's responsibility. This package knows no wallet or SDK types.

## Job queue

Primitives for a durable queue (design 067); the caller composes them and may
replace any.

- `Topic`: what a job locks, dot-separated segments (`n`, `n.w1`). Two
  topics conflict when one is the other or a prefix of it at a dot; the
  meaning of segments is the caller's.
- `Job`: queue fields (`topic`, `kind`, `status`, `run_at`, `deadline`,
  `attempts`, `reason`) and its worker's `input` (never changed) and `state`
  (saved while running).
- `Store`: `enqueue`, `update`, `get`, `next(now)` (due pending jobs with no
  running or earlier unfinished job of a conflicting topic; never overtaken). Implementations live with the application; a store
  returns running jobs to pending when it opens (single process).
- `Changes`: a signal to ask `next` again.
- `Jobs`: jobs in id order with the rules over many: `startable(now)` (what
  `next` returns), `blocker(id)` (what holds a job back), `finished()`.
- `Job::start` and `Job::settle`: the state machine, with `Retry` (attempts,
  backoff × attempt; past the limit `attempts-exhausted: …`; past its
  deadline a job fails `expired`).

With feature `runner` (Tokio):

- `Worker`: one kind; typed `Input` and `State`; `run(Task) -> Ack`. A
  `Task` shows the job, saves its state and answers it (`ack`, `nack`,
  `fail`). A worker must tolerate a rerun.
- `Ticking`: another `Changes` plus a periodic tick, so retries fall due.

The executor (which worker runs a job, how many at once, the loop) is the
caller's.
