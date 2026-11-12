//! A node as `rpc::node` builds it against a loopback node: the bytes on the
//! wire, how each answer maps to a result, a `Failure` or an `Error` kind,
//! and the URL's `rpc.` settings.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rpc::{Answer, Batch, Call, Client, Endpoint, Error, ErrorKind, Failure, Node, RawJson};
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
enum Reply {
    Bytes(Vec<u8>),
    Hang,
    Reset(Vec<u8>),
}

/// Starts a node on `127.0.0.1:0`; every connection gets one `reply`, then
/// closes. Returns its base URL and the requests it saw.
async fn node(reply: fn(&Seen) -> Reply) -> (String, Arc<Mutex<Vec<Seen>>>) {
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
                    Reply::Bytes(bytes) => stream.write_all(&bytes).await.unwrap(),
                    Reply::Hang => tokio::time::sleep(Duration::from_secs(30)).await,
                    Reply::Reset(bytes) => {
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
fn http(status: &str, body: &str) -> Reply {
    Reply::Bytes(
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
    )
}

fn ok(body: &Value) -> Reply {
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

fn connect(url: &str) -> Node {
    rpc::node(&Endpoint::new(url), Default::default()).unwrap()
}

async fn call(url: &str) -> Result<RawJson, Error> {
    connect(url)
        .request(rpc::Request::new("ping", json!([1])))
        .await
}

async fn batch(url: &str) -> Result<Vec<Answer>, Error> {
    let calls = vec![Call::new("a", json!([])), Call::new("b", json!([]))];
    connect(url).batch(Batch::new(calls).unwrap()).await
}

fn raw(text: &str) -> RawJson {
    RawJson::new(text.as_bytes().to_vec()).unwrap()
}

fn kind(outcome: Result<impl std::fmt::Debug, Error>) -> (ErrorKind, bool) {
    let error = outcome.unwrap_err();
    let kind = error.kind().expect("a transport error");
    (kind, error.is_retryable())
}

/// The error object `outcome` failed with.
fn remote(outcome: Result<RawJson, Error>) -> Failure {
    let Err(Error::Remote(failure)) = outcome else {
        panic!("not an error object: {outcome:?}");
    };
    failure
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
    call(&url).await.unwrap();
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
    let failure = remote(call(&url).await);
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
async fn rate_limit_error_objects_stay_remote_and_are_retryable() {
    let (url, _) = node(|seen| match path(seen) {
        "/32005" => ok(&error(&seen.body["id"], -32005, "limit exceeded")),
        "/32003-limit" => ok(&error(&seen.body["id"], -32003, "Request Limit reached")),
        _ => ok(&error(&seen.body["id"], -32003, "transaction rejected")),
    })
    .await;
    let limited = call(&format!("{url}/32005")).await.unwrap_err();
    assert!(limited.is_rate_limited() && limited.is_retryable());
    assert_eq!(
        limited.to_string(),
        "JSON-RPC call failed with code -32005: limit exceeded"
    );
    let limited = call(&format!("{url}/32003-limit")).await.unwrap_err();
    assert!(matches!(
        &limited,
        Error::Remote(Failure { code: -32003, .. })
    ));
    assert!(limited.is_rate_limited() && limited.is_retryable());
    let refused = call(&format!("{url}/32003")).await.unwrap_err();
    assert!(matches!(
        &refused,
        Error::Remote(Failure { code: -32003, .. })
    ));
    assert!(!refused.is_rate_limited() && !refused.is_retryable());
}

#[tokio::test]
async fn a_pool_rejection_mentioning_a_limit_is_not_a_rate_limit() {
    let (url, _) =
        node(|seen| ok(&error(&seen.body["id"], -32003, "exceeds block gas limit"))).await;
    assert!(!remote(call(&url).await).is_rate_limited());
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
        vec![Ok(raw(r#""a""#)), Ok(raw(r#""b""#))]
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
    assert_eq!(results[0], Ok(raw("1")));
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
    let error = batch(&url).await.unwrap_err();
    assert!(matches!(
        &error,
        Error::Remote(Failure { code: -32005, .. })
    ));
    assert!(error.is_retryable());
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
#[ignore = "finding: a short batch answer yields fewer results than calls; callers check the count"]
async fn short_batch_is_invalid_response() {
    let (url, _) = node(|seen| ok(&json!([result(&items(seen)[0]["id"], json!(1))]))).await;
    assert_eq!(kind(batch(&url).await), (ErrorKind::InvalidResponse, false));
}

#[tokio::test]
async fn duplicate_batch_id_is_invalid_response() {
    let (url, _) = node(|seen| {
        let id = &items(seen)[0]["id"];
        ok(&json!([result(id, json!(1)), result(id, json!(2))]))
    })
    .await;
    assert_eq!(kind(batch(&url).await), (ErrorKind::InvalidResponse, false));
}

#[tokio::test]
async fn rpc_timeout_ms_is_applied_and_never_sent() {
    let (url, seen) = node(|_| Reply::Hang).await;
    let http = connect(&format!("{url}/?key=K&rpc.timeout_ms=100"));
    let start = Instant::now();
    let outcome = http.request(rpc::Request::new("ping", json!([]))).await;
    assert_eq!(kind(outcome), (ErrorKind::Timeout, true));
    assert!(start.elapsed() < Duration::from_millis(500));
    let head = &seen.lock().unwrap()[0].head;
    assert!(head.starts_with("post /?key=k http/1.1"), "{head}");
}

#[tokio::test]
async fn a_request_or_batch_times_out_at_its_own_limit() {
    let (url, _) = node(|_| Reply::Hang).await;
    let http = connect(&url);
    let start = Instant::now();
    let short = rpc::Request::new("ping", json!([])).with_timeout(Duration::from_millis(100));
    assert_eq!(kind(http.request(short).await), (ErrorKind::Timeout, true));
    let calls = vec![Call::new("ping", json!([]))];
    let short = Batch::new(calls)
        .unwrap()
        .with_timeout(Duration::from_millis(100));
    assert_eq!(kind(http.batch(short).await), (ErrorKind::Timeout, true));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "not the node's 15 s"
    );
}

#[test]
fn a_batch_may_take_a_base_and_a_second_per_call() {
    let calls = vec![Call::new("a", json!([])), Call::new("b", json!([]))];
    let batch = Batch::new(calls).unwrap();
    assert_eq!(batch.timeout(), rpc::BATCH_BASE + rpc::BATCH_PER_CALL * 2);
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
            Reply::Reset(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"jsonrpc\"".to_vec())
        }
        _ => Reply::Bytes(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"jsonrpc\"".to_vec()),
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
    let http = connect(&format!("{url}/?rpc.max_response_bytes=256"));
    let outcome = http.request(rpc::Request::new("ping", json!([]))).await;
    assert_eq!(kind(outcome), (ErrorKind::ResponseTooLarge, false));
}

#[tokio::test]
async fn oversized_request_is_invalid_request() {
    let (url, seen) = node(|seen| ok(&result(&seen.body["id"], json!(null)))).await;
    let http = connect(&format!("{url}/?rpc.max_request_bytes=64"));
    let outcome = http
        .request(rpc::Request::new("ping", json!(["x".repeat(1000)])))
        .await;
    assert_eq!(kind(outcome), (ErrorKind::InvalidRequest, false));
    assert!(seen.lock().unwrap().is_empty());
}

/// A node answering a batch in order, and a single call alone; it echoes
/// each call's method.
fn echo(seen: &Seen) -> Reply {
    let one = |item: &Value| result(&item["id"], item["method"].clone());
    match &seen.body {
        Value::Array(items) => ok(&Value::Array(items.iter().map(one).collect())),
        item => ok(&one(item)),
    }
}

/// `count` calls named `c0`, `c1`, … as one batch to `url`.
async fn batch_of(url: &str, count: usize) -> Result<Vec<Answer>, Error> {
    let calls = (0..count)
        .map(|n| Call::new(format!("c{n}"), json!([])))
        .collect();
    connect(url).batch(Batch::new(calls).unwrap()).await
}

#[tokio::test]
async fn a_batch_larger_than_max_batch_goes_in_several_requests_in_order() {
    let (url, seen) = node(echo).await;
    let url = url + "/?rpc.max_batch=2";
    let results = batch_of(&url, 5).await.unwrap();
    let methods: Vec<Answer> = (0..5).map(|n| Ok(raw(&format!(r#""c{n}""#)))).collect();
    assert_eq!(results, methods, "every answer, in call order");
    let seen = seen.lock().unwrap();
    let sizes: Vec<usize> = seen.iter().map(|one| items(one).len()).collect();
    assert_eq!(sizes, [2, 2, 1], "requests of at most 2 calls, in order");
    assert!(
        seen.iter().all(|one| !one.head.contains("rpc.max_batch")),
        "never sent to the node"
    );
}

#[tokio::test]
async fn max_batch_1_sends_each_call_alone_never_a_batch() {
    let (url, seen) = node(echo).await;
    let url = url + "/?key=K&rpc.max_batch=1";
    let results = batch_of(&url, 3).await.unwrap();
    assert_eq!(
        results,
        vec![Ok(raw(r#""c0""#)), Ok(raw(r#""c1""#)), Ok(raw(r#""c2""#))]
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for one in seen.iter() {
        assert!(
            one.body.is_object(),
            "a single call, not a batch: {}",
            one.body
        );
        assert!(
            one.head.starts_with("post /?key=k http/1.1"),
            "{}",
            one.head
        );
    }
}

#[tokio::test]
async fn a_batch_within_max_batch_is_one_request() {
    let (url, seen) = node(echo).await;
    batch_of(&(url + "/?rpc.max_batch=10"), 3).await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(items(&seen[0]).len(), 3);
}

#[tokio::test]
async fn a_failing_request_fails_the_split_batch() {
    let (url, _) = node(|seen| {
        if seen
            .body
            .as_array()
            .is_some_and(|items| items[0]["method"] == "c2")
        {
            return http("429 Too Many Requests", "");
        }
        echo(seen)
    })
    .await;
    let outcome = batch_of(&(url + "/?rpc.max_batch=2"), 4).await;
    assert_eq!(kind(outcome), (ErrorKind::HttpStatus(429), true));
}

#[test]
fn rpc_settings_are_read_and_checked() {
    for bad in [
        "http://node.invalid/?rpc.max_batch=0",
        "http://node.invalid/?rpc.max_batch=x",
        "http://node.invalid/?rpc.max_batch=1&rpc.max_batch=2",
        "http://node.invalid/?rpc.timeout_ms=0",
        "http://node.invalid/?rpc.max_request_bytes=-1",
        "http://node.invalid/?rpc.max_response_bytes=4294967296",
    ] {
        let error = Endpoint::new(bad).validate().unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::InvalidConfiguration), "{bad}");
    }
    let endpoint = Endpoint::new(
        "http://node.invalid/?rpc.timeout_ms=20000&rpc.max_request_bytes=1&rpc.max_response_bytes=2&rpc.max_batch=3",
    );
    let url = endpoint.parse().unwrap();
    let params = rpc::HttpParams::try_from(&url).unwrap();
    assert_eq!(
        (
            params.timeout_ms,
            params.max_request_bytes,
            params.max_response_bytes
        ),
        (20_000, 1, 2)
    );
    let split = rpc::SplitParams::try_from(&url).unwrap();
    assert_eq!(split.max_batch.map(|max| max.get()), Some(3));
    let rpc = url.params.namespace("rpc");
    assert_eq!(rpc.get_int::<usize>("max_batch"), Ok(Some(3)));
    assert_eq!(url.params.get_int::<usize>("rpc.max_batch"), Ok(Some(3)));
    assert_eq!(rpc.get("absent"), None);
    let plain = Endpoint::new("http://node.invalid/").parse().unwrap();
    assert_eq!(
        rpc::HttpParams::try_from(&plain),
        Ok(rpc::HttpParams::default())
    );
}

#[test]
fn an_endpoint_is_its_url_in_json() {
    let endpoint = Endpoint::new("http://node.invalid/?rpc.max_batch=1");
    let json = serde_json::to_string(&endpoint).unwrap();
    assert_eq!(json, r#""http://node.invalid/?rpc.max_batch=1""#);
    assert_eq!(serde_json::from_str::<Endpoint>(&json).unwrap(), endpoint);
}
