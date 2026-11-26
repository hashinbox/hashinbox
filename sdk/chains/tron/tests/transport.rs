//! `Rest` against a loopback node: each method's path and body, the URL's
//! headers and query, how a refusal maps, an unknown method.

use std::sync::{Arc, Mutex};

use base::TransactionErrorKind;
use chain_tron::{Client, REFUSAL, Rest, testing::fixtures::config};
use rpc::{Batch, Call, Client as _, Endpoint, Error, ErrorKind, Request};
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

/// Starts a node on `127.0.0.1:0`; every connection gets one answer, by
/// path, then closes. Returns its base URL and the requests it saw.
async fn node(reply: fn(&str) -> Value) -> (String, Arc<Mutex<Vec<Seen>>>) {
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
                let path = request.head.split(' ').nth(1).unwrap().to_owned();
                log.lock().unwrap().push(request);
                let body = reply(&path).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
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

fn rest(url: &str) -> Rest {
    let endpoint = Endpoint::new(url);
    Rest::new(&endpoint.parse().unwrap(), endpoint).unwrap()
}

/// Echoes the path it was asked; a broadcast refused as the path says.
fn answers(path: &str) -> Value {
    let refusal =
        |code: &str, message: &str| json!({"result": false, "code": code, "message": message});
    match path.split('?').next().unwrap() {
        "/v1/wallet/broadcasthex" => refusal("SIGERROR", "Validate signature error"),
        "/dup/wallet/broadcasthex" => refusal("DUP_TRANSACTION_ERROR", "dup transaction"),
        "/expired/wallet/broadcasthex" => refusal("TRANSACTION_EXPIRATION_ERROR", "expired"),
        _ => json!({"path": path}),
    }
}

/// Every method POSTs its params as the body to its path under the node's,
/// the query kept, the URL's headers sent, its `rpc.` and `header.` params
/// not; the body is the answer.
#[tokio::test]
async fn each_method_is_its_rest_call() {
    let (url, seen) = node(answers).await;
    let node = rest(&format!(
        "{url}/v1/?key=K&header.TRON-PRO-API-KEY=SECRET&rpc.timeout_ms=5000"
    ));
    let methods = [
        ("getblock", "/v1/wallet/getblock?key=k"),
        ("getblockbynum", "/v1/wallet/getblockbynum?key=k"),
        (
            "gettransactioninfobyblocknum",
            "/v1/wallet/gettransactioninfobyblocknum?key=k",
        ),
        ("getchainparameters", "/v1/wallet/getchainparameters?key=k"),
        ("solidity_getblock", "/v1/walletsolidity/getblock?key=k"),
    ];
    for (method, path) in methods {
        let body = json!({"num": 7, "detail": false});
        let answer = node
            .request(Request::new(method, body.clone()))
            .await
            .unwrap();
        assert_eq!(
            answer.deserialize::<Value>().unwrap(),
            json!({"path": path})
        );
        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert!(
            last.head.starts_with(&format!("post {path} http/1.1")),
            "{}",
            last.head
        );
        assert!(
            last.head.contains("tron-pro-api-key: secret"),
            "{}",
            last.head
        );
        assert!(
            last.head.contains("content-type: application/json"),
            "{}",
            last.head
        );
        assert_eq!(last.body, body);
    }
    assert_eq!(
        node.endpoint().unwrap().url,
        format!("{url}/v1/?key=K&header.TRON-PRO-API-KEY=SECRET&rpc.timeout_ms=5000")
    );
}

/// An `Error` text and `result: false` are the node's refusal; in a batch
/// it stays with its call.
#[tokio::test]
async fn a_refusal_is_the_nodes_error_object() {
    let (url, _) = node(answers).await;
    let node = rest(&format!("{url}/v1"));
    let broadcast = Request::once("broadcasthex", json!({"transaction": "0a00"}));
    let Err(Error::Remote(refused)) = node.request(broadcast).await else {
        panic!("a refusal");
    };
    assert_eq!(
        (refused.code, refused.message.as_str()),
        (REFUSAL, "SIGERROR: Validate signature error")
    );
    assert!(refused.data.is_some());

    let calls = vec![
        Call::new("getblock", json!({})),
        Call::new("broadcasthex", json!({})),
    ];
    let answers = node.batch(Batch::new(calls).unwrap()).await.unwrap();
    assert!(answers[0].is_ok());
    assert_eq!(answers[1].as_ref().unwrap_err().code, REFUSAL);
}

/// A body naming an `Error` is a refusal whatever the method.
#[tokio::test]
async fn an_error_text_is_a_refusal() {
    let (url, _) = node(|_| json!({"Error": "class java.lang.NullPointerException : null"})).await;
    let failed = rest(&url)
        .request(Request::new("getblock", json!({})))
        .await;
    let Err(Error::Remote(failure)) = failed else {
        panic!("a refusal: {failed:?}");
    };
    assert_eq!(failure.code, REFUSAL);
    assert!(
        failure.message.contains("NullPointerException"),
        "{failure:?}"
    );
}

/// Through the crate's client: a duplicate is accepted, an expired one
/// expired, a bad signature rejected.
#[tokio::test]
async fn a_broadcast_refusal_maps_to_its_outcome() {
    let (url, _) = node(answers).await;
    let send = |path: &str| {
        let client = Client::new(Arc::new(rest(&format!("{url}/{path}"))), config().native());
        async move { client.send(&[0x0a, 0x00]).await }
    };
    assert!(send("dup").await.is_ok());
    assert_eq!(
        send("expired").await.unwrap_err().kind,
        TransactionErrorKind::Expired
    );
    assert_eq!(
        send("v1").await.unwrap_err().kind,
        TransactionErrorKind::Rejected
    );
}

/// A method with no REST call is refused before sending; so are params that
/// are not an object.
#[tokio::test]
async fn an_unknown_method_is_refused_locally() {
    let (url, seen) = node(answers).await;
    let node = rest(&url);
    for request in [
        Request::new("eth_blockNumber", json!({})),
        Request::new("getblock", json!([1])),
    ] {
        let error = node.request(request).await.unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::InvalidRequest));
        assert!(error.is_local());
    }
    let error = node
        .request(Request::new("nope", json!({})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("`nope`"), "{error}");
    assert!(seen.lock().unwrap().is_empty());
}
