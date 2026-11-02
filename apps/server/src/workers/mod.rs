//! The server's background work: the queue's executor, the broadcast job,
//! and triggers (the loop that queues them, and the triggers job). `main.rs` runs them.

pub(crate) mod broadcast;
pub(crate) mod executor;
pub(crate) mod trigger;
