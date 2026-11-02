//! The server's background work, all jobs of one executor ([`executor`]):
//! the index job (per network), the triggers job it queues, the broadcast
//! job those start (design 066), the expire job (per network, design 068)
//! that frees what expired pending transactions held, and the prune job that
//! clears old succeeded jobs. `main.rs` builds the executor and adds them.

pub(crate) mod broadcast;
pub(crate) mod executor;
pub(crate) mod expire;
pub(crate) mod index;
pub(crate) mod prune;
pub(crate) mod trigger;
