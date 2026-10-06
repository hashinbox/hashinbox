use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::Value;

use crate::{Call, CallResult, Error, ErrorKind};

/// Boxed `Send` future returned by [`Client`]; keeps the trait object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Minimal execution boundary used by chain adapters, decorators, and
/// deterministic doubles.
///
/// Every layer has the same contract, so a decorated client is still a
/// `Client`: `Retry::new(Failover::new(vec![http])?)`. What may be executed
/// more than once is a property of the work, not of the layer — the caller
/// states it with [`Repetition`], and no layer may override it.
pub trait Client: Send + Sync {
    /// Executes one request under this implementation's policy.
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>>;

    /// Executes one batch under this implementation's policy.
    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>>;

    /// Executes one request at every node this client reaches, each node
    /// exactly once: the deliberate multi-node path for submitting one signed
    /// envelope. A single node is reached once, so the default is
    /// [`Self::request`]. A decorator forwards this method rather than
    /// inheriting the default, which would silently narrow a broadcast to one
    /// node; [`crate::Cluster`] is the implementation that fans out.
    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        self.request(request)
    }
}

/// A shared client is a client, so one decorated handle serves every chain.
impl<C: Client + ?Sized> Client for Arc<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        (**self).request(request)
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        (**self).batch(batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        (**self).broadcast(request)
    }
}

/// Whether a decorator may execute this work more than once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repetition {
    /// Safe to repeat: reads, and anything the caller can replay.
    Permitted,
    /// Exactly one execution against exactly one endpoint. Raw transaction
    /// submission states this so no layer can hide that an endpoint accepted
    /// the envelope.
    Forbidden,
}

/// One call and whether a decorator may repeat it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    call: Call,
    repetition: Repetition,
}

impl Request {
    /// A repeatable request: retry and failover apply.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            call: Call::new(method, params),
            repetition: Repetition::Permitted,
        }
    }

    /// A request that executes exactly once, against exactly one endpoint.
    #[must_use]
    pub fn once(method: impl Into<String>, params: Value) -> Self {
        Self {
            call: Call::new(method, params),
            repetition: Repetition::Forbidden,
        }
    }

    /// Whether a decorator may execute this request more than once.
    #[must_use]
    pub const fn repetition(&self) -> Repetition {
        self.repetition
    }

    /// The call to execute; parameters are validated by the transport.
    #[must_use]
    pub fn into_call(self) -> Call {
        self.call
    }
}

/// Several calls executed as one JSON-RPC batch, and whether a decorator may
/// repeat them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    calls: Vec<Call>,
    repetition: Repetition,
}

impl Batch {
    /// A repeatable batch; a retry repeats the whole batch, so every call in
    /// it must be safe to repeat.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`] when `calls` is empty.
    pub fn new(calls: Vec<Call>) -> Result<Self, Error> {
        Self::build(calls, Repetition::Permitted)
    }

    /// A batch that executes exactly once, against exactly one endpoint.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`] when `calls` is empty.
    pub fn once(calls: Vec<Call>) -> Result<Self, Error> {
        Self::build(calls, Repetition::Forbidden)
    }

    /// Whether a decorator may execute this batch more than once.
    #[must_use]
    pub const fn repetition(&self) -> Repetition {
        self.repetition
    }

    /// The calls to execute, in request order.
    #[must_use]
    pub fn into_calls(self) -> Vec<Call> {
        self.calls
    }

    fn build(calls: Vec<Call>, repetition: Repetition) -> Result<Self, Error> {
        if calls.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC batch must not be empty",
            ));
        }
        Ok(Self { calls, repetition })
    }
}
