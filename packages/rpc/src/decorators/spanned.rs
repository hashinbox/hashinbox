//! [`Spanned`]: every call of a client inside a span of its own.

use std::sync::Arc;

use tracing::{Instrument as _, Span};

use crate::{Answer, Batch, BoxFuture, Client, Endpoint, Error, RawJson, Request};

/// Makes the span of one call, a child of the span current then. It must
/// return quickly.
pub type MakeSpan = Arc<dyn Fn() -> Span + Send + Sync>;

/// `C`, every call run inside a span of its own made then, a child of the
/// caller's, so each event below it carries the fields of both (a job's,
/// then a node's).
#[derive(Clone)]
pub struct Spanned<C> {
    inner: C,
    span: MakeSpan,
}

impl<C> Spanned<C> {
    /// `inner`, each call inside a span `span` makes.
    #[must_use]
    pub fn new(inner: C, span: MakeSpan) -> Self {
        Self { inner, span }
    }
}

impl<C: Client> Client for Spanned<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(self.inner.request(request).instrument((self.span)()))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.inner.batch(batch).instrument((self.span)()))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.inner.broadcast(request).instrument((self.span)()))
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}
