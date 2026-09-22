use std::{future::Future, pin::Pin};

use serde_json::Value;

use crate::{Call, CallResult, Error};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Minimal execution boundary used by chain adapters and deterministic doubles.
pub trait Client: Send + Sync {
    /// Executes an ordinary request under the implementation's endpoint policy.
    ///
    /// [`crate::Http`] retries retryable transport failures and may fail over to
    /// another endpoint. Callers must use [`Self::request_once`] for operations
    /// that cannot be safely repeated.
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, std::result::Result<CallResult, Error>>;

    /// Executes one request without transport retry or endpoint failover.
    fn request_once<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, std::result::Result<CallResult, Error>>;

    /// Executes a batch under the implementation's endpoint policy.
    ///
    /// [`crate::Http`] may repeat the entire batch after a retryable transport
    /// failure, so every operation in it must be safe to repeat. Received
    /// JSON-RPC call failures and terminal transport errors stop retries.
    fn batch<'a>(
        &'a self,
        calls: Vec<Call>,
    ) -> BoxFuture<'a, std::result::Result<Vec<CallResult>, Error>>;
}
