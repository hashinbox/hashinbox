//! Ordered alternatives: when `Failover` advances, and when it must not.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use json_rpc::{Batch, Call, Client, ErrorKind, Failover, Http, Request};
use serde_json::json;

use crate::node;

fn over(endpoints: [&str; 2]) -> Failover<Http> {
    Failover::new(endpoints.iter().map(|url| node::http(url)).collect())
        .expect("two endpoints are not empty")
}

#[test]
fn an_empty_endpoint_list_is_rejected() {
    let error = Failover::<Http>::new(Vec::new()).expect_err("nothing to call");
    assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
    assert_eq!(error.message, "JSON-RPC requires at least one endpoint");
}

#[tokio::test]
async fn a_transport_failure_advances_to_the_next_endpoint() {
    let answering =
        node::serve_once(|request| json!({"jsonrpc":"2.0", "id":request["id"], "result":42}));
    let result = over([&node::unreachable(), &answering])
        .request(Request::new("answer", json!([])))
        .await
        .expect("second endpoint must be attempted")
        .expect("call must succeed");
    assert_eq!(result.deserialize::<u64>().expect("number must decode"), 42);
}

#[tokio::test]
async fn a_terminal_http_status_stops_at_the_endpoint_that_answered() {
    for batch in [false, true] {
        let executions = Arc::new(AtomicUsize::new(0));
        let selected = node::serve_status_once(Arc::clone(&executions), "400 Bad Request");
        let (listener, fallback) = node::silent();
        let client = over([&selected, &fallback]);
        let error = if batch {
            let calls = Batch::new(vec![Call::new("answer", json!([]))]).expect("one call");
            client.batch(calls).await.err()
        } else {
            client
                .request(Request::new("answer", json!([])))
                .await
                .err()
        }
        .expect("a terminal rejection is not retryable");
        assert_eq!(error.kind, ErrorKind::HttpStatus(400));
        assert_eq!(error.message, "JSON-RPC endpoint rejected the request");
        assert!(!error.is_retryable());
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        node::untouched(&listener);
    }
}

#[tokio::test]
async fn a_received_error_object_stops_at_the_endpoint_that_answered() {
    let executions = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&executions);
    let selected = node::serve_once(move |request| {
        count.fetch_add(1, Ordering::SeqCst);
        json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32001,"message":"no"}})
    });
    let (listener, fallback) = node::silent();
    let failure = over([&selected, &fallback])
        .request(Request::new("answer", json!([])))
        .await
        .expect("a received error object is a result")
        .expect_err("the call failed remotely");
    assert_eq!(failure.code, -32001);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    node::untouched(&listener);
}

#[tokio::test]
async fn forbidden_work_never_leaves_the_first_endpoint() {
    let executions = Arc::new(AtomicUsize::new(0));
    let selected = node::serve_status_once(Arc::clone(&executions), "503 Service Unavailable");
    let (listener, fallback) = node::silent();
    let error = over([&selected, &fallback])
        .request(Request::once("eth_sendRawTransaction", json!(["0x02"])))
        .await
        .expect_err("the selected endpoint's rejection reaches the caller");
    assert_eq!(error.kind, ErrorKind::HttpStatus(503));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    node::untouched(&listener);

    let executions = Arc::new(AtomicUsize::new(0));
    let selected = node::serve_status_once(Arc::clone(&executions), "503 Service Unavailable");
    let (listener, fallback) = node::silent();
    let calls = Batch::new(vec![Call::new("answer", json!([]))]).expect("one call");
    let calls = Batch::once(calls.into_calls()).expect("one call");
    let error = over([&selected, &fallback])
        .batch(calls)
        .await
        .expect_err("a forbidden batch stays on one endpoint");
    assert_eq!(error.kind, ErrorKind::HttpStatus(503));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    node::untouched(&listener);
}

#[tokio::test]
async fn one_round_visits_every_endpoint_in_order_and_keeps_the_last_error() {
    for batch in [false, true] {
        let (events, observed) = std::sync::mpsc::channel();
        let serve = if batch {
            node::serve_batch_rejections::<1>
        } else {
            node::serve_request_rejections::<1>
        };
        let first = serve("first", ["503 Service Unavailable"], events.clone());
        let second = serve("second", ["504 Gateway Timeout"], events);
        let client = over([&first, &second]);
        let error = if batch {
            let calls = Batch::new(vec![Call::new("answer", json!([42]))]).expect("one call");
            client.batch(calls).await.err()
        } else {
            client
                .request(Request::new("answer", json!([42])))
                .await
                .err()
        }
        .expect("one round over two endpoints must exhaust");
        assert_eq!(error.kind, ErrorKind::HttpStatus(504));
        assert_eq!(observed.try_iter().collect::<Vec<_>>(), ["first", "second"]);
    }
}
