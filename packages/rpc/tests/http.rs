//! `Http` alone against a loopback node: the bytes on the wire, and how each
//! answer maps to a result, a `Failure` or an `Error` kind.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rpc::{Batch, Bounds, Call, CallResult, Client, Endpoint, Error, ErrorKind, Failure, Http};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// One request as the node read it: lowercased head and parsed body.
struct Seen {
    head: String,
    body: Value,
}

/// What the node writes back: raw bytes, `None` to hang, `Reset` to abort.
enum Answer {
    Bytes(Vec<u8>),
    Hang,
    Reset(Vec<u8>),
}

/// Starts a node on `127.0.0.1:0`; every connection gets one `reply`, then
/// closes. Returns its base URL and the requests it saw.
async fn node(reply: fn(&Seen) -> Answer) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let request = read(&mut stream).await;
                let answer = reply(&request);
                log.lock().unwrap().push(request);
                match answer {
                    Answer::Bytes(bytes) => stream.write_all(&bytes).await.unwrap(),
                    Answer::Hang => tokio::time::sleep(Duration::from_secs(30)).await,
                    Answer::Reset(bytes) => {
                        stream.write_all(&bytes).await.unwrap();
                        #[allow(deprecated)]
                        stream.set_linger(Some(Duration::ZERO)).unwrap();
                    }
                }
            });
        }
    });
    (url, seen)
}

async fn read(stream: &mut TcpStream) -> Seen {
    let mut buffer = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse().unwrap());
            while buffer.len() < end + 4 + length {
                let read = stream.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body = serde_json::from_slice(&buffer[end + 4..end + 4 + length]).unwrap();
            return Seen { head, body };
        }
        let read = stream.read(&mut chunk).await.unwrap();
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// A full HTTP/1.1 response with a correct `content-length`.
fn http(status: &str, body: &str) -> Answer {
    Answer::Bytes(
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
    )
}

fn ok(body: &Value) -> Answer {
    http("200 OK", &body.to_string())
}

fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": {"why": "x"}}})
}

/// The request path; the tests route on it.
fn path(seen: &Seen) -> &str {
    seen.head.split(' ').nth(1).unwrap()
}

fn items(seen: &Seen) -> &Vec<Value> {
    seen.body.as_array().unwrap()
}

fn connect(url: &str, bounds: Bounds) -> Http {
    Endpoint::new(url).with_bounds(bounds).connect().unwrap()
}

async fn call(url: &str) -> Result<CallResult, Error> {
    connect(url, Bounds::default())
        .request(rpc::Request::new("ping", json!([1])))
        .await
}

async fn batch(url: &str) -> Result<Vec<CallResult>, Error> {
    let calls = vec![Call::new("a", json!([])), Call::new("b", json!([]))];
    connect(url, Bounds::default())
        .batch(Batch::new(calls).unwrap())
        .await
}

fn raw(text: &str) -> CallResult {
    Ok(rpc::RawJson::new(text.as_bytes().to_vec()).unwrap())
}

fn kind(outcome: Result<impl std::fmt::Debug, Error>) -> (ErrorKind, bool) {
    let error = outcome.unwrap_err();
    (error.kind, error.is_retryable())
}

#[tokio::test]
async fn single_call_returns_result_bytes() {
    let (url, seen) = node(|seen| ok(&result(&seen.body["id"], json!({"a": 1})))).await;
    assert_eq!(call(&url).await.unwrap(), raw(r#"{"a":1}"#));
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].body["jsonrpc"], "2.0");
    assert_eq!(seen[0].body["method"], "ping");
    assert_eq!(seen[0].body["params"], json!([1]));
    assert!(seen[0].head.starts_with("post / http/1.1"));
}

#[tokio::test]
async fn request_carries_json_content_type_auth_and_header_parameters() {
    let (url, seen) = node(|seen| ok(&result(&seen.body["id"], json!(null)))).await;
    let url = url.replace("http://", "http://user:pass@") + "/v1?key=K&header.x-api-key=SECRET";
    call(&url).await.unwrap().unwrap();
    let seen = seen.lock().unwrap();
    let head = &seen[0].head;
    assert!(head.starts_with("post /v1?key=k http/1.1"), "{head}");
    assert!(head.contains("content-type: application/json"), "{head}");
    assert!(head.contains("authorization: basic dxnlcjpwyxnz"), "{head}");
    assert!(head.contains("x-api-key: secret"), "{head}");
    assert!(!head.contains("header.x-api-key"), "{head}");
}

#[tokio::test]
async fn error_object_keeps_code_message_and_data() {
    let (url, _) = node(|seen| ok(&error(&seen.body["id"], -32000, "header not found"))).await;
    let failure = call(&url).await.unwrap().unwrap_err();
    assert_eq!(
        failure,
        Failure {
            code: -32000,
            message: "header not found".into(),
            data: Some(rpc::RawJson::new(br#"{"why":"x"}"#.to_vec()).unwrap()),
        }
    );
    assert!(failure.is_server_error());
    assert_eq!(
        failure.to_string(),
        "JSON-RPC call failed with code -32000: header not found"
    );
}

#[tokio::test]
async fn rate_limit_error_objects_are_retryable_transport_errors() {
    let (url, _) = node(|seen| match path(seen) {
        "/32005" => ok(&error(&seen.body["id"], -32005, "limit exceeded")),
        "/32003-limit" => ok(&error(&seen.body["id"], -32003, "Request Limit reached")),
        _ => ok(&error(&seen.body["id"], -32003, "transaction rejected")),
    })
    .await;
    let limited = call(&format!("{url}/32005")).await.unwrap_err();
    assert_eq!(limited.kind, ErrorKind::RateLimited(-32005));
    assert!(limited.is_retryable());
    assert_eq!(
        limited.message,
        "JSON-RPC call failed with code -32005: limit exceeded"
    );
    let outcome = call(&format!("{url}/32003-limit")).await;
    assert_eq!(kind(outcome), (ErrorKind::RateLimited(-32003), true));
    let failure = call(&format!("{url}/32003")).await.unwrap().unwrap_err();
    assert_eq!(failure.code, -32003);
}

#[tokio::test]
async fn batch_results_follow_request_order() {
    let (url, seen) = node(|seen| {
        let mut out: Vec<Value> = items(seen)
            .iter()
            .map(|item| result(&item["id"], item["method"].clone()))
            .collect();
        out.reverse();
        ok(&Value::Array(out))
    })
    .await;
    assert_eq!(
        batch(&url).await.unwrap(),
        vec![raw(r#""a""#), raw(r#""b""#)]
    );
    assert_eq!(items(&seen.lock().unwrap()[0]).len(), 2);
}

#[tokio::test]
async fn batch_with_one_failed_item() {
    let (url, _) = node(|seen| {
        let calls = items(seen);
        ok(&json!([
            result(&calls[0]["id"], json!(1)),
            error(&calls[1]["id"], -32601, "method not found"),
        ]))
    })
    .await;
    let results = batch(&url).await.unwrap();
    assert_eq!(results[0], raw("1"));
    assert_eq!(results[1].as_ref().unwrap_err().code, -32601);
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn batch_with_one_rate_limited_item_fails_whole() {
    let (url, _) = node(|seen| {
        let calls = items(seen);
        ok(&json!([
            result(&calls[0]["id"], json!(1)),
            error(&calls[1]["id"], -32005, "limit exceeded"),
        ]))
    })
    .await;
    assert_eq!(
        kind(batch(&url).await),
        (ErrorKind::RateLimited(-32005), true)
    );
}

#[tokio::test]
async fn http_status_maps_to_kind_and_retryability() {
    let (url, _) = node(|seen| {
        let status = &path(seen)[1..];
        http(&format!("{status} X"), "<html>busy</html>")
    })
    .await;
    for (status, retryable) in [
        (429, true),
        (502, true),
        (503, true),
        (504, true),
        (500, false),
        (404, false),
    ] {
        let outcome = call(&format!("{url}/{status}")).await;
        assert_eq!(
            kind(outcome),
            (ErrorKind::HttpStatus(status), retryable),
            "{status}"
        );
    }
}

/// The status wins over a JSON-RPC body: the error object is dropped.
#[tokio::test]
async fn status_500_with_json_rpc_body_is_http_status() {
    let (url, _) = node(|seen| {
        http(
            "500 Internal Server Error",
            &error(&seen.body["id"], -32000, "boom").to_string(),
        )
    })
    .await;
    assert_eq!(kind(call(&url).await), (ErrorKind::HttpStatus(500), false));
}

#[tokio::test]
async fn malformed_bodies_are_invalid_response() {
    let (url, _) = node(|seen| match path(seen) {
        "/invalid" => http("200 OK", "not json"),
        "/truncated" => http("200 OK", r#"{"jsonrpc":"2.0","id":0,"res"#),
        "/empty" => http("200 OK", ""),
        "/wrong-id" => ok(&result(&json!(999), json!(1))),
        _ => ok(&json!({"jsonrpc": "2.0", "id": seen.body["id"]})),
    })
    .await;
    for case in ["invalid", "truncated", "empty", "wrong-id", "no-result"] {
        let outcome = call(&format!("{url}/{case}")).await;
        assert_eq!(kind(outcome), (ErrorKind::InvalidResponse, false), "{case}");
    }
}

#[tokio::test]
async fn batch_with_unknown_id_is_invalid_response() {
    let (url, _) = node(|seen| {
        let calls = items(seen);
        ok(&json!([
            result(&calls[0]["id"], json!(1)),
            result(&json!(999), json!(2))
        ]))
    })
    .await;
    assert_eq!(kind(batch(&url).await), (ErrorKind::InvalidResponse, false));
}

#[tokio::test]
#[ignore = "finding: a short batch answer yields fewer results than calls instead of an error"]
async fn short_batch_is_invalid_response() {
    let (url, _) = node(|seen| ok(&json!([result(&items(seen)[0]["id"], json!(1))]))).await;
    assert_eq!(kind(batch(&url).await), (ErrorKind::InvalidResponse, false));
}

#[tokio::test]
#[ignore = "finding: a duplicated batch id fabricates Failure { code: 0, message: \"\" } for the missing call"]
async fn duplicate_batch_id_is_invalid_response() {
    let (url, _) = node(|seen| {
        let id = &items(seen)[0]["id"];
        ok(&json!([result(id, json!(1)), result(id, json!(2))]))
    })
    .await;
    assert_eq!(kind(batch(&url).await), (ErrorKind::InvalidResponse, false));
}

#[tokio::test]
async fn slow_node_times_out() {
    let (url, _) = node(|_| Answer::Hang).await;
    let http = connect(
        &url,
        Bounds {
            timeout_ms: 100,
            ..Bounds::default()
        },
    );
    let start = Instant::now();
    let outcome = http.request(rpc::Request::new("ping", json!([]))).await;
    assert_eq!(kind(outcome), (ErrorKind::Timeout, true));
    assert!(start.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn refused_connection_is_unavailable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(kind(call(&url).await), (ErrorKind::Unavailable, true));
}

#[tokio::test]
async fn connection_cut_mid_response_is_unavailable() {
    let (url, _) = node(|seen| match path(seen) {
        "/reset" => {
            Answer::Reset(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"jsonrpc\"".to_vec())
        }
        _ => Answer::Bytes(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"jsonrpc\"".to_vec()),
    })
    .await;
    for case in ["reset", "closed"] {
        let outcome = call(&format!("{url}/{case}")).await;
        assert_eq!(kind(outcome), (ErrorKind::Unavailable, true), "{case}");
    }
}

#[tokio::test]
async fn oversized_response_is_too_large() {
    let (url, _) = node(|seen| ok(&result(&seen.body["id"], json!("x".repeat(1000))))).await;
    let http = connect(
        &url,
        Bounds {
            max_response_bytes: 256,
            ..Bounds::default()
        },
    );
    let outcome = http.request(rpc::Request::new("ping", json!([]))).await;
    assert_eq!(kind(outcome), (ErrorKind::ResponseTooLarge, false));
}

#[tokio::test]
#[ignore = "finding: an oversized request fails locally but maps to retryable Unavailable; expected InvalidRequest"]
async fn oversized_request_is_invalid_request() {
    let (url, seen) = node(|seen| ok(&result(&seen.body["id"], json!(null)))).await;
    let http = connect(
        &url,
        Bounds {
            max_request_bytes: 64,
            ..Bounds::default()
        },
    );
    let outcome = http
        .request(rpc::Request::new("ping", json!(["x".repeat(1000)])))
        .await;
    assert_eq!(kind(outcome), (ErrorKind::InvalidRequest, false));
    assert!(seen.lock().unwrap().is_empty());
}
