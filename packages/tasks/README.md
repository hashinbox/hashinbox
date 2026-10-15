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

`Store` is the durable queue contract; implementations live with the
application (the server's SQLite queue over `schema/queue.sql`). It keeps
`Job`s whole: `set` inserts or replaces one (payload wholesale) and publishes
it to `changes`; `get` reads one; `next(now)` lists unfinished jobs due by
`now`, by id. A job has a lane, a kind, a `State` (pending, running, done,
failed), a JSON object `payload` (input plus whatever is saved) and `run_at`.

Every rule is the `Runner`'s (feature `runner`):

- A lane runs one job at a time, oldest first; there is no lease.
- On start, running jobs return to pending (single process: they were left
  by a dead worker), so executors must tolerate a rerun.
- It wakes on a change, an ended execution or the earliest `run_at`/deadline.
- Payload keys it owns: `attempts`, `reason` and optional `deadline` (ms; a
  pending job at or past it fails as `expired`).
- Executors get the `Job` and the store (to save: `set` the running job with
  a new payload) and answer `Ack::Done`, `Fail(reason)` or `Retry(reason)`
  (backoff × attempt; past the limit `attempts-exhausted: …`). An unknown kind
  fails (`unknown-kind`), a panic fails (`panicked`).

`Handle::wait` waits for a job to finish using `get` and `changes`.
