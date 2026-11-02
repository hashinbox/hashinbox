//! The server's background work, all jobs of one executor: the index job
//! (per network), the triggers job it queues, the broadcast job those
//! start, and the prune job that clears old succeeded jobs. `main.rs` runs
//! them.

pub(crate) mod broadcast;
pub(crate) mod executor;
pub(crate) mod indexer;
pub(crate) mod prune;
pub(crate) mod trigger;
