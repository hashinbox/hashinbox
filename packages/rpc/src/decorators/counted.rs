//! Per-node execution outcomes, readable without locks or I/O, and
//! [`Counted`], which records them.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{Answer, Batch, BoxFuture, Client, Endpoint, Error, RawJson, Request};

/// A shared handle to one node's execution counters.
/// Clones observe the same counters; separate nodes do not.
#[derive(Clone, Debug, Default)]
pub struct Statistics(Arc<Counters>);

#[derive(Debug, Default)]
struct Counters {
    attempts: AtomicU64,
    failures: AtomicU64,
    consecutive_failures: AtomicU64,
    last_failure: AtomicU64,
}

/// One nonblocking sample of a node's outcomes.
/// Fields are sampled independently while requests run; after they settle all
/// outcomes are included. A cancelled in-flight execution counts nowhere.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Executions that reached the transport and finished; a batch counts
    /// once, and work refused before sending (an invalid parameter) never.
    pub attempts: u64,
    /// Executions with a transport error or at least one JSON-RPC error response.
    pub failures: u64,
    /// Failed completions since the latest successful completion.
    pub consecutive_failures: u64,
    /// Last failure's sanitized classification or remote code, retained after success.
    /// Never contains a URL, header, request, remote message or response data.
    pub last_failure: Option<String>,
}

/// An execution's outcome, as [`Counted`] and [`crate::Logged`] read it.
pub(crate) trait Outcome {
    /// What failed; `None` on success. A batch fails by its first error object.
    fn failure(&self) -> Option<Error>;
}

impl Outcome for Result<RawJson, Error> {
    fn failure(&self) -> Option<Error> {
        self.as_ref().err().cloned()
    }
}

impl Outcome for Result<(), Error> {
    fn failure(&self) -> Option<Error> {
        self.as_ref().err().cloned()
    }
}

impl Outcome for Result<Vec<Answer>, Error> {
    fn failure(&self) -> Option<Error> {
        match self {
            Err(error) => Some(error.clone()),
            Ok(answers) => answers
                .iter()
                .find_map(|answer| answer.as_ref().err())
                .cloned()
                .map(Error::Remote),
        }
    }
}

impl Statistics {
    /// Reads counters from memory without acquiring a lock or making a request.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let code = self.0.last_failure.load(Ordering::Relaxed);
        Snapshot {
            attempts: self.0.attempts.load(Ordering::Relaxed),
            failures: self.0.failures.load(Ordering::Relaxed),
            consecutive_failures: self.0.consecutive_failures.load(Ordering::Relaxed),
            last_failure: Error::describe(code),
        }
    }

    /// Records one finished execution; work refused before sending is none.
    fn record(&self, failure: Option<Error>) {
        if failure.as_ref().is_some_and(Error::is_local) {
            return;
        }
        Self::increment(&self.0.attempts);
        let Some(error) = failure else {
            self.0.consecutive_failures.store(0, Ordering::Relaxed);
            return;
        };
        self.0.last_failure.store(error.code(), Ordering::Relaxed);
        Self::increment(&self.0.failures);
        Self::increment(&self.0.consecutive_failures);
    }

    fn increment(counter: &AtomicU64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(1))
        });
    }
}

/// `C`, its outcomes counted into a [`Statistics`] every clone shares: one
/// logical call, batch or broadcast is one execution, however `C` sends it.
#[derive(Clone, Debug)]
pub struct Counted<C> {
    inner: C,
    statistics: Statistics,
}

impl<C> Counted<C> {
    /// `inner`, counted from zero.
    #[must_use]
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            statistics: Statistics::default(),
        }
    }

    /// The counters, shared by every clone.
    #[must_use]
    pub fn statistics(&self) -> Statistics {
        self.statistics.clone()
    }

    async fn count<T: Outcome>(&self, sent: impl Future<Output = T>) -> T {
        let outcome = sent.await;
        self.statistics.record(outcome.failure());
        outcome
    }
}

impl<C: Client> Client for Counted<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(self.count(self.inner.request(request)))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.count(self.inner.batch(batch)))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.count(self.inner.broadcast(request)))
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}

#[cfg(test)]
mod tests {
    use futures_executor::block_on;
    use serde_json::json;

    use super::{Counted, Snapshot};
    use crate::{
        Client, Endpoint, ErrorKind, Request,
        testing::{Fault, Faults, Reply, Script},
    };

    fn snapshot(attempts: u64, failures: u64, consecutive: u64, last: Option<&str>) -> Snapshot {
        Snapshot {
            attempts,
            failures,
            consecutive_failures: consecutive,
            last_failure: last.map(str::to_owned),
        }
    }

    #[test]
    fn each_outcome_counts_as_its_kind() {
        let script = Script::new([
            Reply::ok("m", json!(1)),
            Reply::failure("m", -32_000, "reverted"),
        ]);
        let faults = Faults::new(script);
        let switch = faults.switch();
        let counted = Counted::new(faults);
        let ask = || block_on(counted.request(Request::new("m", json!([]))));
        ask().unwrap();
        assert_eq!(counted.statistics().snapshot(), snapshot(1, 0, 0, None));
        ask().unwrap_err();
        let remote = Some("JSON-RPC call failed with code -32000");
        assert_eq!(counted.statistics().snapshot(), snapshot(2, 1, 1, remote));
        switch.fail_next(None, Fault::Transport(ErrorKind::Timeout));
        ask().unwrap_err();
        let timeout = Some("JSON-RPC request timed out");
        assert_eq!(counted.statistics().snapshot(), snapshot(3, 2, 2, timeout));
        switch.fail_next(
            None,
            Fault::Remote(-32_005, "Rate limit exceeded".to_owned()),
        );
        ask().unwrap_err();
        let limited = Some("JSON-RPC call failed with code -32005");
        assert_eq!(counted.statistics().snapshot(), snapshot(4, 3, 3, limited));
    }

    #[test]
    fn a_broadcast_counts_and_clones_share_counters() {
        let counted = Counted::new(Script::new([Reply::ok("send", json!("id"))]));
        let clone = counted.clone();
        block_on(clone.broadcast(Request::once("send", json!([])))).unwrap();
        assert_eq!(counted.statistics().snapshot(), snapshot(1, 0, 0, None));
    }

    #[tokio::test]
    async fn an_invalid_parameter_is_not_counted() {
        let node = crate::node(&Endpoint::new("http://127.0.0.1:9/"), Default::default()).unwrap();
        let outcome = node.request(Request::new("m", json!(1))).await;
        assert_eq!(outcome.unwrap_err().kind(), Some(ErrorKind::InvalidRequest));
        assert_eq!(node.statistics().snapshot(), Snapshot::default());
    }
}
