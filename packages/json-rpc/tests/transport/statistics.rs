//! Counters measure endpoint executions beneath retry and failover decorators.

use crate::node;
use json_rpc::{Backoff, Batch, Call, Client, Failover, Request, Retry, Snapshot};
use serde_json::json;
use std::{num::NonZeroU32, sync::mpsc, time::Duration};

#[tokio::test]
async fn invalid_inputs_are_not_attempts_and_node_errors_are_failures() {
    let endpoint = node::serve_once(|request| {
        json!({
            "jsonrpc":"2.0", "id":request["id"],
            "error":{"code":-32001, "message":"hidden credential"}
        })
    });
    let client = node::http(&endpoint);
    let statistics = client.statistics();
    assert!(
        client
            .request(Request::new("bad", json!(null)))
            .await
            .is_err()
    );
    let invalid = Batch::new(vec![Call::new("bad", json!(null))]).expect("batch");
    assert!(client.batch(invalid).await.is_err());
    assert_eq!(statistics.snapshot(), Snapshot::default());
    assert!(
        client
            .clone()
            .request(Request::once("answer", json!([])))
            .await
            .expect("envelope")
            .is_err()
    );
    let snapshot = statistics.snapshot();
    assert_eq!((snapshot.attempts, snapshot.failures), (1, 1));
    assert_eq!(
        snapshot.last_failure.as_deref(),
        Some("JSON-RPC call failed with code -32001")
    );
}

#[tokio::test]
async fn each_retry_round_records_only_the_endpoints_it_reaches() {
    let (events, observed) = mpsc::channel();
    let first = node::serve_request_rejections("first", ["503 Unavailable"; 2], events.clone());
    let second = node::serve_request_rejections("second", ["502 Gateway"; 2], events);
    let first = node::http(&first);
    let second = node::http(&second);
    let handles = [first.statistics(), second.statistics()];
    let client = Retry::new(Failover::new(vec![first, second]).expect("endpoints"))
        .with_attempts(NonZeroU32::new(2).expect("attempts"))
        .with_backoff(Backoff::Fixed(Duration::ZERO));
    assert!(
        client
            .request(Request::new("answer", json!([42])))
            .await
            .is_err()
    );
    assert_eq!(observed.try_iter().count(), 4);
    for handle in handles {
        let snapshot = handle.snapshot();
        assert_eq!(
            (
                snapshot.attempts,
                snapshot.failures,
                snapshot.consecutive_failures
            ),
            (2, 2, 2)
        );
    }
}

#[tokio::test]
async fn once_only_never_increments_a_fallback_counter() {
    let (events, observed) = mpsc::channel();
    let endpoint = node::serve_request_rejections("first", ["503 Unavailable"], events);
    let (listener, untouched) = node::silent();
    let first = node::http(&endpoint);
    let second = node::http(&untouched);
    let handles = [first.statistics(), second.statistics()];
    let client = Retry::new(Failover::new(vec![first, second]).expect("endpoints"))
        .with_backoff(Backoff::Fixed(Duration::ZERO));
    assert!(
        client
            .request(Request::once("answer", json!([42])))
            .await
            .is_err()
    );
    assert_eq!(observed.try_iter().count(), 1);
    assert_eq!(handles[0].snapshot().attempts, 1);
    assert_eq!(handles[1].snapshot(), Snapshot::default());
    node::untouched(&listener);
}

#[tokio::test]
async fn injected_logs_name_the_node_and_never_include_provider_prose_or_payloads() {
    use std::sync::Arc;
    let logs = Arc::new(logging::Memory::default());
    let logger = logging::Logger::new(logs.clone()).with("node_id", "primary");
    let endpoint = node::serve_once(|request| {
        json!({"jsonrpc":"2.0", "id":request["id"],
        "error":{"code":-32001,"message":"private credential", "data":"private payload"}})
    });
    let client = node::http(&endpoint).with_logger(logger);
    let _ = client
        .request(Request::once("private-method", json!(["private input"])))
        .await
        .unwrap();
    let page = logs.page(0, 10, &logging::Filter::default());
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].fields["node_id"], "primary");
    assert!(page.entries[0].fields["kind"].contains("-32001"));
    assert!(!format!("{page:?}").contains("private"));
}
