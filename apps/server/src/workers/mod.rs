//! The server's background work, all jobs of one executor: the index job
//! (per network), the triggers job it queues, and the broadcast job those
//! start. `main.rs` runs them.

pub(crate) mod broadcast;
pub(crate) mod executor;
pub(crate) mod indexer;
pub(crate) mod trigger;
