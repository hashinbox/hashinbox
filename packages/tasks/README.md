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
