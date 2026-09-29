//! Repetition: `Retry(Failover(Http))` is the composition the server builds,
//! and an attempt is one complete round over the endpoint list.

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use json_rpc::{
    Backoff, Batch, BoxFuture, Call, CallResult, Client, Error, ErrorKind, Failover, Failure, Http,
    RawJson, Repetition, Request, Retry, Trigger,
};
use serde_json::json;

use crate::node;

/// The composition `apps/server` builds, with instant backoff.
fn retrying(endpoints: [&str; 2], attempts: u32) -> Retry<Failover<Http>> {
    let failover = Failover::new(endpoints.iter().map(|url| node::http(url)).collect())
        .expect("two endpoints are not empty");
    Retry::new(failover)
        .with_attempts(NonZeroU32::new(attempts).expect("a positive attempt count"))
        .with_backoff(Backoff::Fixed(Duration::ZERO))
}

#[tokio::test]
async fn an_attempt_is_one_round_over_the_endpoint_list() {
    for batch in [false, true] {
        let (events, observed) = mpsc::channel();
        let serve = if batch {
            node::serve_batch_rejections::<2>
        } else {
            node::serve_request_rejections::<2>
        };
        let first = serve(
            "first",
            ["503 Service Unavailable", "429 Too Many Requests"],
            events.clone(),
        );
        let second = serve("second", ["502 Bad Gateway", "504 Gateway Timeout"], events);
        let client = retrying([&first, &second], 2);
        let error = if batch {
            let calls = Batch::new(vec![Call::new("answer", json!([42]))]).expect("one call");
            client.batch(calls).await.err()
        } else {
            client
                .request(Request::new("answer", json!([42])))
                .await
                .err()
        }
        .expect("both rounds must exhaust");
        assert_eq!(error.kind, ErrorKind::HttpStatus(504));
        assert_eq!(error.message, "JSON-RPC endpoint rejected the request");
        assert!(error.is_retryable());
        assert_eq!(
            observed.try_iter().collect::<Vec<_>>(),
            ["first", "second", "first", "second"]
        );
    }
}

#[tokio::test]
async fn the_default_policy_makes_three_rounds() {
    for batch in [false, true] {
        let (events, observed) = mpsc::channel();
        let serve = if batch {
            node::serve_batch_rejections::<3>
        } else {
            node::serve_request_rejections::<3>
        };
        let first = serve("first", ["503 Service Unavailable"; 3], events.clone());
        let second = serve("second", ["504 Gateway Timeout"; 3], events);
        let failover = Failover::new(vec![node::http(&first), node::http(&second)])
            .expect("two endpoints are not empty");
        let client = Retry::new(failover).with_backoff(Backoff::Fixed(Duration::ZERO));
        let error = if batch {
            let calls = Batch::new(vec![Call::new("answer", json!([42]))]).expect("one call");
            client.batch(calls).await.err()
        } else {
            client
                .request(Request::new("answer", json!([42])))
                .await
                .err()
        }
        .expect("the default must exhaust three rounds");
        assert_eq!(error.kind, ErrorKind::HttpStatus(504));
        assert_eq!(
            observed.try_iter().collect::<Vec<_>>(),
            ["first", "second", "first", "second", "first", "second"]
        );
    }
}

#[tokio::test]
async fn forbidden_work_reaches_exactly_one_endpoint_once() {
    let executions = Arc::new(AtomicUsize::new(0));
    let selected = node::serve_status_once(Arc::clone(&executions), "503 Service Unavailable");
    let (listener, fallback) = node::silent();
    let error = retrying([&selected, &fallback], 3)
        .request(Request::once("eth_sendRawTransaction", json!(["0x02"])))
        .await
        .expect_err("one attempt, one endpoint");
    assert_eq!(error.kind, ErrorKind::HttpStatus(503));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    node::untouched(&listener);
}

/// A node that always answers with the same JSON-RPC error object.
struct Rejecting {
    code: i64,
    calls: Arc<AtomicUsize>,
}

impl Client for Rejecting {
    fn request<'a>(&'a self, _request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let failure = Failure {
            code: self.code,
            message: "provider prose".to_owned(),
            data: None,
        };
        Box::pin(async move { Ok(Err(failure)) })
    }

    fn batch<'a>(&'a self, _batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[tokio::test]
async fn a_trigger_widens_which_answers_are_repeated() {
    for (trigger, code, expected) in [
        (Trigger::Transport, -32_000, 1),
        (Trigger::Transport, -28, 1),
        (Trigger::Server, -32_000, 3),
        (Trigger::Server, -28, 1),
        (Trigger::Remote(vec![-28]), -28, 3),
        (Trigger::Remote(vec![-28]), -26, 1),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let node = Rejecting {
            code,
            calls: Arc::clone(&calls),
        };
        let failure = Retry::new(node)
            .with_backoff(Backoff::Fixed(Duration::ZERO))
            .with_trigger(trigger.clone())
            .request(Request::new("answer", json!([])))
            .await
            .expect("a received error object is a result")
            .expect_err("the node always rejects");
        assert_eq!(failure.code, code);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            expected,
            "{trigger:?} over code {code}"
        );
    }
}

/// A node that fails the first `failures` calls, then answers.
struct Flaky {
    failures: usize,
    calls: Arc<AtomicUsize>,
}

impl Client for Flaky {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let repetition = request.repetition();
        Box::pin(async move {
            assert_eq!(repetition, Repetition::Permitted);
            if call < self.failures {
                return Err(Error {
                    kind: ErrorKind::Timeout,
                    message: "timed out".to_owned(),
                });
            }
            Ok(Ok(RawJson::from_serializable(&call)?))
        })
    }

    fn batch<'a>(&'a self, _batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[tokio::test]
async fn a_transport_failure_is_repeated_up_to_the_attempt_count() {
    for (failures, attempts, succeeds) in [(1_usize, 2_u32, true), (2, 2, false), (2, 3, true)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let node = Flaky {
            failures,
            calls: Arc::clone(&calls),
        };
        let outcome = Retry::new(node)
            .with_attempts(NonZeroU32::new(attempts).expect("a positive attempt count"))
            .with_backoff(Backoff::Fixed(Duration::ZERO))
            .request(Request::new("answer", json!([])))
            .await;
        assert_eq!(outcome.is_ok(), succeeds);
        let expected = if succeeds {
            failures + 1
        } else {
            attempts as usize
        };
        assert_eq!(calls.load(Ordering::SeqCst), expected);
    }
}
