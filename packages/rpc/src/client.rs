use std::{future::Future, num::NonZeroUsize, pin::Pin, sync::Arc, time::Duration};

use serde_json::Value;

use crate::{Call, Error, ErrorKind, Failure, RawJson};

/// One call's outcome in a [`Batch`]: its result, or the node's error object
/// for that call alone.
pub type Answer = Result<RawJson, Failure>;

/// A call's outcome as a batch's [`Answer`]: an error object stays with its
/// call; a rate limit fails the batch, as one in a sent batch does, and so
/// does a transport error.
pub(crate) fn answer(outcome: Result<RawJson, Error>) -> Result<Answer, Error> {
    match outcome {
        Ok(raw) => Ok(Ok(raw)),
        Err(error) if error.is_rate_limited() => Err(error),
        Err(Error::Remote(failure)) => Ok(Err(failure)),
        Err(error) => Err(error),
    }
}

/// Boxed `Send` future returned by [`Client`]; keeps the trait object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Minimal execution boundary used by adapters, decorators, and
/// deterministic doubles.
///
/// Every layer has the same contract, so a decorated client is still a
/// `Client`: `Retry::new(node)`. What may be executed
/// more than once is a property of the work, not of the layer — the caller
/// states it with [`Repetition`], and no layer may override it.
pub trait Client: Send + Sync {
    /// Executes one request under this implementation's policy.
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>>;

    /// Executes one batch under this implementation's policy: one outcome
    /// per call, in call order, since one call can fail alone.
    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>>;

    /// Sends `request` (a signed transaction) once to every node this client
    /// reaches: success or failure only, since the transaction's id is known
    /// before it is sent. A node's error object is [`Error::Remote`]. A
    /// single node is reached once, so the default is [`Self::request`]. A
    /// decorator forwards this method rather than inheriting the default,
    /// which would silently narrow a broadcast to one node of a client that
    /// fans out.
    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move { self.request(request).await.map(drop) })
    }
}

/// A shared client is a client, so one decorated handle serves every holder.
impl<C: Client + ?Sized> Client for Arc<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        (**self).request(request)
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        (**self).batch(batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
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

/// One call, whether a decorator may repeat it, and how long one execution
/// may take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    call: Call,
    repetition: Repetition,
    timeout: Option<Duration>,
}

impl Request {
    /// A repeatable request: retry and routing apply.
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

    /// The method it calls.
    pub(crate) fn method(&self) -> &str {
        &self.call.method
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

    /// The method every call names, if they all name one.
    pub(crate) fn method(&self) -> Option<&str> {
        let (first, rest) = self.calls.split_first()?;
        rest.iter()
            .all(|call| call.method == first.method)
            .then_some(first.method.as_str())
    }

    /// Batches of at most `max` calls, in order, each keeping this one's
    /// repetition and timeout.
    pub(crate) fn chunks(self, max: NonZeroUsize) -> Vec<Self> {
        self.calls
            .chunks(max.get())
            .map(|calls| Self {
                calls: calls.to_vec(),
                ..self
            })
            .collect()
    }

    /// Each call as a request of its own, keeping this one's repetition and
    /// timeout.
    pub(crate) fn requests(self) -> Vec<Request> {
        self.calls
            .into_iter()
            .map(|call| Request {
                call,
                repetition: self.repetition,
                timeout: Some(self.timeout),
            })
            .collect()
    }

    fn build(calls: Vec<Call>, repetition: Repetition) -> Result<Self, Error> {
        if calls.is_empty() {
            return Err(Error::transport(
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
