//! The server's background work, one file each: the send queue (its job
//! input and state, executor and lane rule) and the trigger loop. `main.rs`
//! runs them.

pub(crate) mod send;
pub(crate) mod triggers;
