#![doc = include_str!("../README.md")]

mod client;
mod cluster;
mod endpoint;
mod error;
mod failover;
mod http;
mod retry;
mod statistics;
mod swap;
#[cfg(feature = "testing")]
pub mod testing;
mod value;

use serde_json::Value;

pub use client::{Batch, BoxFuture, Client, Repetition, Request};
pub use cluster::{Cluster, Limit, Policy};
pub use endpoint::{Bounds, Endpoint};
pub use error::{CallError, Error, ErrorKind};
pub use failover::Failover;
pub use http::Http;
pub use retry::{Backoff, Retry, Trigger};
pub use statistics::{Snapshot, Statistics};
pub use swap::Swap;
pub use value::{CallResult, Failure, RawJson};

/// One request of a [`Batch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// JSON-RPC method name.
    pub method: String,
    /// Positional (array) or named (object) parameters; anything else is rejected.
    pub params: Value,
}

impl Call {
    /// Builds a call; parameters are validated when the call executes.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

mod events;
