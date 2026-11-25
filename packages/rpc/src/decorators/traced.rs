//! [`Traced`]: every HTTP request a node is sent, as one `rpc.http.*` event,
//! and `rpc.slow` while one is pending.

use std::{future::Future, time::Duration};

use tokio::time::Instant;
use tracing::Instrument as _;

use super::{tally::Tally, throttle::waited};
use crate::{
    Answer, Batch, BoxFuture, Client, Endpoint, Error, ErrorKind, MakeSpan, Outcome, RawJson,
    Request, events,
};

/// When a pending request is first logged as slow.
const SLOW: Duration = Duration::from_secs(5);

/// How often a request still pending after that is logged again.
const SLOW_AGAIN: Duration = Duration::from_secs(15);

/// `C`, each request it sends logged (`method batch ms wait_ms status rpc_errors
/// error`) in its own context (`span`, e.g. the node's network and id):
/// `rpc.http.answered` (debug) when it succeeded, `rpc.http.not_yet` (debug)
/// when the node is not at the block asked yet ([`Error::is_not_ready`]),
/// `rpc.http.broken` (error) when the node answered HTTP 5xx,
/// `rpc.http.failed` (warn) when anything else failed, an error object
/// inside a 200 batch too. `ms` is its time on the wire,
/// `wait_ms` its time in a [`crate::Throttle`] above before it was sent; both
/// go into the [`Tally`] kept, if any. A request still pending after 5 s is
/// logged as `rpc.slow` (`method waited_ms`), warn, then every 15 s. The
/// outcome is unaltered; it holds no state. Work refused before sending is
/// no request. Put it right on [`crate::Http`], so each event is one
/// request as sent.
#[derive(Clone)]
pub struct Traced<C> {
    inner: C,
    span: MakeSpan,
    slow: Duration,
    slow_again: Duration,
}

impl<C> Traced<C> {
    /// `inner`, each request logged in a fresh `span()`, a child of the
    /// caller's.
    #[must_use]
    pub fn new(inner: C, span: MakeSpan) -> Self {
        Self {
            inner,
            span,
            slow: SLOW,
            slow_again: SLOW_AGAIN,
        }
    }

    /// A pending request logged as slow after `first`, then every `again`
    /// (tests).
    #[must_use]
    pub const fn with_slow(mut self, first: Duration, again: Duration) -> Self {
        self.slow = first;
        self.slow_again = again;
        self
    }

    /// `sent`, one request of `method` (`batch`: its calls), logged.
    async fn trace<T>(&self, method: &str, batch: Option<usize>, sent: impl Future<Output = T>) -> T
    where
        T: Outcome,
    {
        let (wait_ms, started) = (millis(waited()), Instant::now());
        let outcome = self.pending(method, started, sent).await;
        let errors = outcome.errors();
        if errors.iter().any(Error::is_local) {
            return outcome;
        }
        let ms = millis(started.elapsed());
        Tally::add(ms, wait_ms);
        let shown = |value: Option<String>| value.unwrap_or_else(|| "-".to_owned());
        let codes: Vec<String> = errors
            .iter()
            .filter_map(|error| match error {
                Error::Remote(failure) => Some(failure.code.to_string()),
                Error::Transport { .. } => None,
            })
            .collect();
        let transport = errors.iter().find(|error| error.kind().is_some());
        let status = match transport.and_then(Error::kind) {
            None => Some(200),
            Some(ErrorKind::HttpStatus(status)) => Some(status),
            Some(_) => None,
        };
        let batch = shown(batch.map(|calls| calls.to_string()));
        let status = shown(status.map(|status| status.to_string()));
        let rpc_errors = shown((!codes.is_empty()).then(|| codes.join(",")));
        let error = shown(transport.map(Error::label));
        let server_error = matches!(
            transport.and_then(Error::kind),
            Some(ErrorKind::HttpStatus(status)) if status >= 500
        );
        if server_error {
            // The node itself broke (HTTP 5xx).
            tracing::error!(
                target: events::RPC_HTTP_BROKEN,
                method, batch, ms, wait_ms, status, rpc_errors, error
            );
        } else if errors.iter().any(|error| !error.is_not_ready()) {
            tracing::warn!(
                target: events::RPC_HTTP_FAILED,
                method, batch, ms, wait_ms, status, rpc_errors, error
            );
        } else if errors.is_empty() {
            tracing::debug!(
                target: events::RPC_HTTP_ANSWERED,
                method, batch, ms, wait_ms, status, rpc_errors, error
            );
        } else {
            tracing::debug!(
                target: events::RPC_HTTP_NOT_YET,
                method, batch, ms, wait_ms, status, rpc_errors, error
            );
        }
        outcome
    }

    /// `sent`, logged as `rpc.slow` while it is pending past `slow`, then
    /// every `slow_again`.
    async fn pending<T>(&self, method: &str, started: Instant, sent: impl Future<Output = T>) -> T {
        let mut sent = std::pin::pin!(sent);
        let mut due = started + self.slow;
        loop {
            tokio::select! {
                outcome = &mut sent => return outcome,
                () = tokio::time::sleep_until(due) => {
                    let waited_ms = millis(started.elapsed());
                    tracing::warn!(target: events::RPC_SLOW, method, waited_ms);
                    due += self.slow_again;
                }
            }
        }
    }
}

impl<C: Client> Client for Traced<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        let span = (self.span)();
        Box::pin(
            async move {
                let method = request.method().to_owned();
                self.trace(&method, None, self.inner.request(request)).await
            }
            .instrument(span),
        )
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        let span = (self.span)();
        Box::pin(
            async move {
                let method = batch.method().unwrap_or("batch").to_owned();
                let calls = batch.len();
                self.trace(&method, Some(calls), self.inner.batch(batch))
                    .await
            }
            .instrument(span),
        )
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        let span = (self.span)();
        Box::pin(
            async move {
                let method = request.method().to_owned();
                self.trace(&method, None, self.inner.broadcast(request))
                    .await
            }
            .instrument(span),
        )
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}

/// `duration` in whole ms.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use serde_json::json;
    use tracing::{
        Event, Subscriber,
        field::{Field, Visit},
    };

    use super::Traced;
    use crate::{
        Answer, Batch, BoxFuture, Client, Error, RawJson, Request, events,
        testing::{Reply, Script},
    };

    /// A node that answers `inner` after `delay`.
    struct Late {
        inner: Script,
        delay: Duration,
    }

    impl Client for Late {
        fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                self.inner.request(request).await
            })
        }

        fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
            self.inner.batch(batch)
        }
    }

    /// Each event's target and level.
    #[derive(Clone, Default)]
    struct Heard(Arc<Mutex<Vec<(String, tracing::Level)>>>);

    struct Ignored;

    impl Visit for Ignored {
        fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
    }

    impl Subscriber for Heard {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &Event<'_>) {
            event.record(&mut Ignored);
            let metadata = event.metadata();
            let heard = (metadata.target().to_owned(), *metadata.level());
            self.0.lock().unwrap().push(heard);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[tokio::test]
    async fn a_pending_request_is_logged_slow_and_not_ready_is_debug() {
        let script = Script::new([
            Reply::ok("m", json!(1)),
            Reply::failure(
                "eth_getLogs",
                -32_602,
                "block range extends beyond current head block",
            ),
        ]);
        let late = Late {
            inner: script,
            delay: Duration::from_millis(120),
        };
        let traced = Traced::new(late, Arc::new(tracing::Span::none))
            .with_slow(Duration::from_millis(30), Duration::from_millis(50));
        let heard = Heard::default();
        let _default = tracing::subscriber::set_default(heard.clone());
        traced.request(Request::new("m", json!([]))).await.unwrap();
        let ahead = traced.request(Request::new("eth_getLogs", json!([])));
        assert!(ahead.await.unwrap_err().is_not_ready());
        let heard = heard.0.lock().unwrap().clone();
        let slow = heard
            .iter()
            .filter(|(target, _)| target == events::RPC_SLOW);
        assert!(slow.clone().count() >= 2, "{heard:?}");
        assert!(
            slow.clone()
                .all(|(_, level)| *level == tracing::Level::WARN)
        );
        let http: Vec<_> = heard
            .iter()
            .filter(|(target, _)| target.starts_with("rpc.http."))
            .map(|(target, level)| (target.as_str(), *level))
            .collect();
        let debug = tracing::Level::DEBUG;
        let codes = [events::RPC_HTTP_ANSWERED, events::RPC_HTTP_NOT_YET];
        assert_eq!(http, codes.map(|code| (code, debug)), "{heard:?}");
    }
}
