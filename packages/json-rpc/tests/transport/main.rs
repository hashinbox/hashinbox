//! The transport's behavior against real loopback nodes: one HTTP execution
//! ([`http`]), the URL that describes a node ([`endpoint`]), and the two
//! decorators that add repetition ([`retry`]) and ordered alternatives
//! ([`failover`]); the two failure shapes a caller flattens ([`error`]); and
//! the scripted node a chain adapter substitutes for all of it ([`script`]).

mod endpoint;
mod error;
mod failover;
mod http;
mod node;
mod retry;
mod script;
