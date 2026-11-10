# tasks

A job queue: a worker per kind of job, a store with an in-memory default,
and an executor running a queue per worker.

## Usage

```rust
use std::time::Duration;

use tasks::{Ack, Admission, Context, Executor, MemoryStore, Notifying, Policy, Topic, Worker};

/// Greets one name; one greeting per name at a time.
struct Greet;

impl Worker for Greet {
    const KIND: &'static str = "greet";
    const POLICY: Policy = Policy::DEFAULT;
    type Input = String;
    type State = ();

    fn topic(name: &String) -> Option<Topic> {
        Topic::new(["greet", name.as_str()])
    }

    async fn run(&self, cx: Context<'_, String, ()>) -> Ack {
        println!("hello {}", cx.input());
        Ack::Done
    }
}

# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), tasks::StoreError> {
let store = Notifying::new(MemoryStore::new());
let running = Executor::new(store.clone(), Duration::from_millis(10))
    .add(Greet)
    .run();
let Admission::Queued(id) = Greet::enqueue(&store, &"ann".to_owned()).await? else {
    unreachable!("a new topic");
};
# loop {
#     use tasks::Store as _;
#     if store.get(id).await?.is_some_and(|job| job.status.is_finished()) { break; }
#     tokio::time::sleep(Duration::from_millis(5)).await;
# }
running.stop().await;
# Ok(())
# }
```

## Pieces

- `Job`: its `id` (its place in line, given by the store), `topic` (what it
  locks), `kind`, `status`, `run_at`, `deadline`, `attempts`, `reason`, its
  worker's `input` and `state`.
- `Topic`: dot-separated segments; two topics conflict when one is the other
  or a prefix of it at a dot (`n` locks `n.w1`; `n.w1` and `n.w2` do not lock
  each other).
- `Worker`: `KIND`, `POLICY`, `topic(input)`, `run(cx) -> Ack`, and
  optionally `before(store)`, run when its queue starts, where it enqueues
  the jobs that must always exist;
  `W::job(input)` and `W::enqueue(store, input)` come with it. A worker must
  tolerate a rerun; `cx.save(state)` stores how far it got. A recurring job
  is one that answers `Ack::Again(at, why)` instead of `Done`.
- `Policy`: `on_lock` (`Wait` in line, or `Skip`: give way to a newer
  conflicting job), `unique` (one unfinished job of the kind per topic),
  `concurrency`, `timeout`, `retry`.
- `Store`: `enqueue`, `claim`, `update`, `get`. It holds the queue's rules:
  line order, conflicts, skips, expiry and leases. `MemoryStore` is the
  reference; a durable store runs the same rules in its query language.
- `Notifying`: a store that tells each kind's queue when to look again.
- `Executor`: a `Queue` per worker; `run()` starts them and returns
  `Running`, which `stop()`s them.
