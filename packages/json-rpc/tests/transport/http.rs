//! One endpoint, one execution: what `Http` does and what it refuses to do.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use json_rpc::{Batch, Bounds, Call, Client, Endpoint, ErrorKind, Request};
use serde_json::{Value, json};

use crate::node;

#[test]
fn debug_never_exposes_userinfo_or_query_values() {
    let endpoint = Endpoint::new("https://user:config-secret@example.invalid/v1?key=config-query")
        .with_bounds(Bounds {
            timeout_ms: 1_000,
            ..Bounds::default()
        });
    for debug in [
        format!("{endpoint:?}"),
        endpoint.to_string(),
        format!(
            "{:?}",
            endpoint.connect().expect("valid endpoint must build")
        ),
    ] {
        assert!(
            debug.contains("https://***@example.invalid/v1?key=***"),
            "{debug}"
        );
        assert!(!debug.contains("config-secret"));
        assert!(!debug.contains("config-query"));
        assert!(!debug.contains("user"));
    }
}

#[tokio::test]
async fn jsonrpsee_restores_batch_order() {
    let endpoint = node::serve_once(|request| {
        let calls = request.as_array().expect("batch request must be an array");
        json!([
            {"jsonrpc":"2.0", "id":calls[1]["id"], "result":"second"},
            {"jsonrpc":"2.0", "id":calls[0]["id"], "result":"first"}
        ])
    });
    let batch = Batch::new(vec![
        Call::new("first", json!([])),
        Call::new("second", json!([])),
    ])
    .expect("a batch with calls");
    let results = node::http(&endpoint)
        .batch(batch)
        .await
        .expect("batch must succeed");
    let values = results
        .into_iter()
        .map(|result| {
            result
                .expect("call must succeed")
                .deserialize::<String>()
                .expect("string must decode")
        })
        .collect::<Vec<_>>();
    assert_eq!(values, ["first", "second"]);
}

#[test]
fn an_empty_batch_is_rejected_before_any_client_exists() {
    for outcome in [Batch::new(Vec::new()), Batch::once(Vec::new())] {
        let error = outcome.expect_err("an empty batch has nothing to execute");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(error.message, "JSON-RPC batch must not be empty");
        assert!(!error.is_retryable());
    }
}

#[tokio::test]
async fn invalid_parameters_fail_before_anything_is_sent() {
    let (listener, endpoint) = node::silent();
    let client = node::http(&endpoint);
    for params in [Value::Null, json!(true), json!(42), json!("invalid")] {
        let pending = client.request(Request::new("answer", params.clone()));
        node::untouched(&listener);
        let error = pending
            .await
            .expect_err("scalar parameters must fail locally");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert_eq!(
            error.message,
            "JSON-RPC parameters must be an array or object"
        );
        assert!(!error.is_retryable());

        let batch = Batch::new(vec![
            Call::new("valid", json!([])),
            Call::new("bad", params),
        ])
        .expect("a batch with calls");
        let pending = client.batch(batch);
        node::untouched(&listener);
        let error = pending.await.expect_err("one invalid call fails the batch");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        node::untouched(&listener);
    }
}

#[tokio::test]
async fn raw_results_and_call_failures_reach_the_caller_byte_for_byte() {
    const RAW: &str = r#"{ "amount": 1.2300, "large": 18446744073709551616, "tag": "\u0061" }"#;
    for failed in [false, true] {
        let executions = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&executions);
        let payload = if failed {
            format!(r#""error":{{"code":-32001,"message":"call rejected","data":{RAW}}}"#)
        } else {
            format!(r#""result":{RAW}"#)
        };
        let endpoint = node::serve_once(move |request| {
            count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request["method"], "answer");
            assert_eq!(request["params"], json!({"input":42}));
            serde_json::value::RawValue::from_string(format!(
                r#"{{"jsonrpc":"2.0","id":{}, {payload}}}"#,
                request["id"],
            ))
            .expect("exact wire response")
        });
        let result = node::http(&endpoint)
            .request(Request::new("answer", json!({"input":42})))
            .await
            .expect("a received call result is not a transport failure");
        match result {
            Ok(value) => {
                assert!(!failed);
                assert_eq!(value.as_bytes(), RAW.as_bytes());
            }
            Err(error) => {
                assert!(failed);
                assert_eq!(error.code, -32001);
                assert_eq!(error.message, "call rejected");
                assert_eq!(error.data.unwrap().as_bytes(), RAW.as_bytes());
            }
        }
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn a_batch_keeps_every_per_call_failure_as_a_result() {
    let endpoint = node::serve_once(|request| {
        json!([
            {"jsonrpc":"2.0", "id":request[1]["id"], "error":{
                "code":-32001, "message":"call rejected", "data":{"reason":"retained"}
            }},
            {"jsonrpc":"2.0", "id":request[0]["id"], "result":"accepted"}
        ])
    });
    let batch = Batch::new(vec![
        Call::new("success", json!([])),
        Call::new("failure", json!({"input":42})),
    ])
    .expect("a batch with calls");
    let results = node::http(&endpoint)
        .batch(batch)
        .await
        .expect("per-call failure must remain a batch result");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].as_ref().unwrap().as_bytes(), br#""accepted""#);
    let failure = results[1].as_ref().unwrap_err();
    assert_eq!(failure.code, -32001);
    assert_eq!(failure.message, "call rejected");
    assert_eq!(
        failure.data.as_ref().unwrap().as_bytes(),
        br#"{"reason":"retained"}"#
    );
}

#[tokio::test]
async fn repetition_makes_no_difference_to_one_endpoint() {
    for request in [
        Request::new("answer", json!([])),
        Request::once("answer", json!([])),
    ] {
        let endpoint =
            node::serve_once(|body| json!({"jsonrpc":"2.0", "id":body["id"], "result":42}));
        let value = node::http(&endpoint)
            .request(request)
            .await
            .expect("one execution")
            .expect("call must succeed");
        assert_eq!(value.deserialize::<u64>().expect("number"), 42);
    }
}

#[tokio::test]
async fn an_oversized_response_fails_after_one_execution() {
    let executions = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&executions);
    let endpoint = node::serve_once(move |request| {
        count.fetch_add(1, Ordering::SeqCst);
        json!({"jsonrpc":"2.0", "id":request["id"], "result":"x".repeat(256)})
    });
    let client = Endpoint::new(endpoint)
        .with_bounds(Bounds {
            max_response_bytes: 64,
            ..Bounds::default()
        })
        .connect()
        .expect("loopback client must build");
    let error = client
        .request(Request::once("oversized", json!([])))
        .await
        .expect_err("oversized response must fail");
    assert_eq!(error.kind, ErrorKind::ResponseTooLarge);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_mismatched_wire_id_is_an_invalid_response() {
    let endpoint = node::serve_once(|request| {
        let request_id = request["id"]
            .as_u64()
            .expect("request wire ID must be an unsigned integer");
        let response_id = request_id
            .checked_add(1)
            .expect("test request ID must have a successor");
        json!({"jsonrpc":"2.0", "id":response_id, "result":"wrong"})
    });
    let error = node::http(&endpoint)
        .request(Request::once("correlated", json!([])))
        .await
        .expect_err("response ID must match the request wire ID");
    assert_eq!(error.kind, ErrorKind::InvalidResponse);
}

#[tokio::test]
async fn a_request_can_be_cancelled_after_one_execution() {
    let node::Held {
        endpoint,
        observed,
        release,
        thread,
    } = node::serve_held();
    let client = node::http(&endpoint);
    let request =
        tokio::spawn(async move { client.request(Request::once("held", json!([]))).await });
    let observed = tokio::task::spawn_blocking(move || {
        observed
            .recv_timeout(Duration::from_secs(2))
            .expect("one request must reach the server")
    })
    .await
    .expect("request observer must not panic");
    assert_eq!(observed["method"], "held");

    request.abort();
    let cancelled = request
        .await
        .expect_err("aborted request task must report cancellation");
    assert!(cancelled.is_cancelled());
    release
        .send(())
        .expect("held loopback response must be released");
    tokio::task::spawn_blocking(move || {
        thread.join().expect("held loopback server must not panic");
    })
    .await
    .expect("held loopback join must not panic");
}

#[test]
fn remote_failure_conversion_preserves_code_and_message() {
    use jsonrpsee::types::ErrorObjectOwned;

    for code in [i32::MIN, -32_000, 0, i32::MAX] {
        let native =
            ErrorObjectOwned::owned(code, "remote \"message\"\nwith unicode: λ", None::<()>);
        let failure = json_rpc::Failure::from(native);
        assert_eq!(failure.code, i64::from(code));
        assert_eq!(failure.message, "remote \"message\"\nwith unicode: λ");
        assert_eq!(failure.data, None);
    }
}

#[test]
fn remote_failure_conversion_preserves_explicit_null_and_raw_data_bytes() {
    use jsonrpsee::types::ErrorObjectOwned;
    use serde_json::value::RawValue;

    for raw in [
        "null",
        concat!(
            r#"{ "amount": 1.2300, "large": 18446744073709551616, "tag": "\u0061""#,
            r#", "items": [true, null] }"#,
        ),
        r#"[ 1, "two", false ]"#,
        r#""remote data""#,
    ] {
        let data = RawValue::from_string(raw.to_owned()).unwrap();
        let native = ErrorObjectOwned::owned(-32_001, "retained error data", Some(data));
        let failure = json_rpc::Failure::from(native);
        assert_eq!(failure.data.unwrap().as_bytes(), raw.as_bytes());
    }
}
