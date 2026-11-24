//! [`Guard`]: a stored node turned off, through its [`NodeSwitch`], when it
//! answers what it cannot recover from.

use std::sync::Arc;

use rpc::{
    Answer, Batch, BoxFuture, Client, Endpoint, Error, ErrorKind, Outcome, RawJson, Request,
};

use crate::usecase::nodes::{NodeSwitch, unrecoverable};

/// `C`, guarded: while its switch is off every call is refused at once
/// with a local error ([`ErrorKind::InvalidRequest`]), so the transport
/// moves on; the first outcome it cannot recover from (HTTP 401, 403, 404,
/// or an error object of such a code, also inside a batch) turns it off.
/// Anything else passes through untouched.
pub(crate) struct Guard<C> {
    inner: C,
    switch: Arc<NodeSwitch>,
}

impl<C> Guard<C> {
    /// `inner`, turned off by `switch`.
    pub(crate) const fn new(inner: C, switch: Arc<NodeSwitch>) -> Self {
        Self { inner, switch }
    }

    async fn guard<T>(&self, sent: BoxFuture<'_, Result<T, Error>>) -> Result<T, Error>
    where
        Result<T, Error>: Outcome,
    {
        if self.switch.is_off() {
            return Err(Error::Transport {
                kind: ErrorKind::InvalidRequest,
                message: "the node is disabled".to_owned(),
            });
        }
        let outcome = sent.await;
        let id = self.switch.endpoint_id();
        let errors = outcome.errors();
        if let Some(failure) = errors.iter().find_map(|error| unrecoverable(id, error)) {
            self.switch.turn_off(failure).await;
        }
        outcome
    }
}

impl<C: Client> Client for Guard<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(self.guard(self.inner.request(request)))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.guard(self.inner.batch(batch)))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.guard(self.inner.broadcast(request)))
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}
