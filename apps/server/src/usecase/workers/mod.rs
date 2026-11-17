//! The server's background work, all jobs of one `tasks::Executor`
//! (which `main.rs` builds): the index job (per network), the triggers job it queues, the
//! broadcast job those start (designs 066, 084: one per payment, which it
//! owns until mined at a final block or proven dead), and the prune job that
//! clears old succeeded jobs. Each worker declares its `tasks::Policy`
//! (design 076).

use std::{num::NonZeroU32, time::Duration};

use tasks::Retry;

pub(crate) mod broadcast;
pub(crate) mod index;
pub(crate) mod prune;
pub(crate) mod trigger;

/// The longest run of every kind but index, until per-method RPC budgets
/// set one per kind; past it the run is dropped and retried.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);

/// The retry of a job that must keep recurring (index, prune):
/// attempts never run out, each retry waiting a second longer.
pub(crate) const FOREVER: Retry = Retry {
    attempts: NonZeroU32::MAX,
    backoff: Duration::from_secs(1),
};
