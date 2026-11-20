//! Test support for the queue: [`store_contract`],
//! every [`Store`] rule as one test each, and running jobs by hand
//! ([`claim`], [`run_once`], [`end`]).
//!
//! Behind the `testing` feature: dev-dependencies only.

#[doc(hidden)]
pub mod contract;

pub use crate::store_contract;

use std::sync::Arc;

use crate::{Ack, Claim, Context, Id, Job, OnLock, Store, Worker};

/// Claims up to `limit` jobs of `kind` at `now`, as the executor would:
/// waiting on locks, leased for 60 s.
///
/// # Panics
/// When the store fails.
pub async fn claim(store: &impl Store, kind: &str, limit: usize, now: u64) -> Vec<Job> {
    let claim = Claim {
        kind,
        limit,
        on_lock: OnLock::Wait,
        now,
        until: now + 60_000,
    };
    store.claim(claim).await.expect("claimed")
}

/// One run of job `id` by `worker`, as stored now; how it ended is not
/// stored ([`end`]).
///
/// # Panics
/// When the store fails or knows no job `id`.
pub async fn run_once<W: Worker, S: Store + Clone + 'static>(worker: &W, store: &S, id: Id) -> Ack {
    let mut job = stored(store, id).await;
    let shared: Arc<dyn Store> = Arc::new(store.clone());
    match Context::new(&mut job, shared) {
        Ok(cx) => worker.run(cx).await,
        Err(ack) => ack,
    }
}

/// Stores how job `id`'s run ended, under `W`'s policy.
///
/// # Panics
/// When the store fails or knows no job `id`.
pub async fn end<W: Worker>(store: &impl Store, id: Id, ack: Ack) {
    let job = stored(store, id).await;
    let attempt = job.attempts;
    let ended = job.settle(ack, &W::POLICY, crate::now());
    store.update(ended, attempt).await.expect("stored");
}

/// The stored job `id`.
async fn stored(store: &impl Store, id: Id) -> Job {
    store.get(id).await.expect("a read").expect("the job")
}
