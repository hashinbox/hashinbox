use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use serde_json::Value;

use crate::{Call, CallError, CallResult, Error, ErrorKind};

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

    /// Sends `request` (a signed transaction) once to every node this client
    /// reaches: success or failure only, since the transaction's id is known
    /// before it is sent. A node's error object is [`CallError::Remote`]. A
    /// single node is reached once, so the default is [`Self::request`]. A
    /// decorator forwards this method rather than inheriting the default,
    /// which would silently narrow a broadcast to one node;
    /// [`crate::Cluster`] is the implementation that fans out.
    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move { narrow(self.request(request).await) })
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

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>> {
        (**self).broadcast(request)
    }
}

/// A call's outcome as a broadcast's: the answer is dropped.
pub(crate) fn narrow(outcome: Result<CallResult, Error>) -> Result<(), CallError> {
    match outcome {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(failure)) => Err(CallError::Remote(failure)),
        Err(error) => Err(CallError::Transport(error)),
    }
}

/// A broadcast's outcome as a call's, for the layers that judge calls.
pub(crate) fn widen(outcome: Result<(), CallError>) -> Result<CallResult, Error> {
    match outcome {
        Ok(()) => Ok(Ok(crate::RawJson(b"null".to_vec()))),
        Err(CallError::Remote(failure)) => Ok(Err(failure)),
        Err(CallError::Transport(error)) => Err(error),
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

/// One call, whether a decorator may repeat it, and how long one execution
/// may take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    call: Call,
    repetition: Repetition,
    timeout: Option<Duration>,
}

impl Request {
    /// A repeatable request: retry and failover apply.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            call: Call::new(method, params),
            repetition: Repetition::Permitted,
            timeout: None,
        }
    }

    /// A request that executes exactly once, against exactly one endpoint.
    #[must_use]
    pub fn once(method: impl Into<String>, params: Value) -> Self {
        Self {
            call: Call::new(method, params),
            repetition: Repetition::Forbidden,
            timeout: None,
        }
    }

    /// The same, one execution taking at most `timeout`; the node's own
    /// bound still applies when it is shorter.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// How long one execution may take; `None`: the node's bound.
    #[must_use]
    pub const fn timeout(&self) -> Option<Duration> {
        self.timeout
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

/// Several calls executed as one JSON-RPC batch, whether a decorator may
/// repeat them, and how long one execution may take: by default
/// [`BATCH_BASE`] plus [`BATCH_PER_CALL`] per call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    calls: Vec<Call>,
    repetition: Repetition,
    timeout: Duration,
}

/// What any batch may take, before its calls.
pub const BATCH_BASE: Duration = Duration::from_secs(5);

/// What each call adds to a batch's timeout.
pub const BATCH_PER_CALL: Duration = Duration::from_secs(1);

/// What a tip probe may take: a node slower than this is not one to pick.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// What one node may take to answer a submission.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// The same, one execution taking at most `timeout`; the node's own
    /// bound still applies when it is shorter.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How long one execution may take.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
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
        let count = u32::try_from(calls.len()).unwrap_or(u32::MAX);
        let timeout = BATCH_BASE.saturating_add(BATCH_PER_CALL.saturating_mul(count));
        Ok(Self {
            calls,
            repetition,
            timeout,
        })
    }
}
