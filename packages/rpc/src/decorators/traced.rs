//! [`Traced`]: every HTTP request a node is sent, as one `rpc.http` event.

use std::time::Instant;

use tracing::Instrument as _;

use crate::{
    Answer, Batch, BoxFuture, Client, Endpoint, Error, ErrorKind, MakeSpan, RawJson, Request,
};

/// `C`, each request it sends logged as `rpc.http` (`method batch ms status
/// rpc_errors error`) in its own context (`span`, e.g. the node's network
/// and id): debug when it succeeded, warn when anything failed,
/// an error object inside a 200 batch too. The outcome is unaltered; it
/// holds no state. Work refused before sending is no request. Put it right
/// on [`crate::Http`], so each event is one request as sent.
#[derive(Clone)]
pub struct Traced<C> {
    inner: C,
    span: MakeSpan,
}

impl<C> Traced<C> {
    /// `inner`, each request logged in a fresh `span()`, a child of the
    /// caller's.
    #[must_use]
    pub fn new(inner: C, span: MakeSpan) -> Self {
        Self { inner, span }
    }
}

impl<C: Client> Client for Traced<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        let span = (self.span)();
        Box::pin(
            async move {
                let (method, started) = (request.method().to_owned(), Instant::now());
                let outcome = self.inner.request(request).await;
                trace(&method, None, started, outcome.as_ref().err(), &[]);
                outcome
            }
            .instrument(span),
        )
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        let span = (self.span)();
        Box::pin(
            async move {
                let method = batch.method().unwrap_or("batch").to_owned();
                let (calls, started) = (batch.len(), Instant::now());
                let outcome = self.inner.batch(batch).await;
                let codes: Vec<i64> = match &outcome {
                    Ok(answers) => answers
                        .iter()
                        .filter_map(|answer| answer.as_ref().err().map(|failure| failure.code))
                        .collect(),
                    Err(_) => Vec::new(),
                };
                trace(
                    &method,
                    Some(calls),
                    started,
                    outcome.as_ref().err(),
                    &codes,
                );
                outcome
            }
            .instrument(span),
        )
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        let span = (self.span)();
        Box::pin(
            async move {
                let (method, started) = (request.method().to_owned(), Instant::now());
                let outcome = self.inner.broadcast(request).await;
                trace(&method, None, started, outcome.as_ref().err(), &[]);
                outcome
            }
            .instrument(span),
        )
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}

/// Logs one request of `method` (`batch`: its calls) sent at `started`,
/// failed with `error` or answered with error objects `codes`: see
/// [`Traced`].
fn trace(
    method: &str,
    batch: Option<usize>,
    started: Instant,
    error: Option<&Error>,
    codes: &[i64],
) {
    if error.is_some_and(Error::is_local) {
        return;
    }
    let shown = |value: Option<String>| value.unwrap_or_else(|| "-".to_owned());
    let mut codes = codes.to_vec();
    if let Some(Error::Remote(failure)) = error {
        codes.push(failure.code);
    }
    let status = match error.and_then(Error::kind) {
        None => Some(200),
        Some(ErrorKind::HttpStatus(status)) => Some(status),
        Some(_) => None,
    };
    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let batch = shown(batch.map(|calls| calls.to_string()));
    let status = shown(status.map(|status| status.to_string()));
    let joined: Vec<String> = codes.iter().map(i64::to_string).collect();
    let rpc_errors = shown((!joined.is_empty()).then(|| joined.join(",")));
    let kind = error
        .filter(|error| error.kind().is_some())
        .map(Error::label);
    let failed = kind.is_some() || !codes.is_empty();
    let error = shown(kind);
    if failed {
        tracing::warn!(
            target: "rpc.http",
            method, batch, ms, status, rpc_errors, error,
            "RPC request failed"
        );
    } else {
        tracing::debug!(
            target: "rpc.http",
            method, batch, ms, status, rpc_errors, error,
            "RPC request answered"
        );
    }
}
