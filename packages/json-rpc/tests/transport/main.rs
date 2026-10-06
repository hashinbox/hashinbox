//! The transport's behavior against real loopback nodes: one HTTP execution
//! ([`http`]), the URL that describes a node ([`endpoint`]), and the two
//! decorators that add repetition ([`retry`]) and ordered alternatives
//! ([`failover`]); the managed composite that selects a primary and
//! broadcasts ([`cluster`]); the handle whose nodes are replaced
//! while it is held ([`swap`]); the two failure shapes a caller flattens
//! ([`error`]); and the scripted node a chain adapter substitutes for all of
//! it ([`script`]).

mod cluster;
mod endpoint;
mod error;
mod failover;
mod http;
mod node;
mod retry;
mod script;
mod statistics;
mod swap;
