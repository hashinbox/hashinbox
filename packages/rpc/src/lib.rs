#![doc = include_str!("../README.md")]

mod client;
mod decorators;
mod endpoint;
mod error;
mod http;
mod node;
#[cfg(feature = "testing")]
pub mod testing;
mod value;

use serde_json::Value;

pub use client::{
    Answer, BATCH_BASE, BATCH_PER_CALL, Batch, BoxFuture, Client, Repetition, Request, SEND_TIMEOUT,
};
pub use decorators::{
    Backoff, Counted, Logged, Retry, Router, Snapshot, Split, SplitParams, Statistics, Swap,
};
pub use endpoint::{Endpoint, Parameter, Params, Parts, Url};
pub use error::{Error, ErrorKind, Failure};
pub use http::{Http, HttpParams};
pub use node::{Node, node};
pub use value::RawJson;

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
