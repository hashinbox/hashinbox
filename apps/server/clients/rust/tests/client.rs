//! The reqwest client against a loopback server: the bytes it sends (path,
//! query, body, bearer), how answers map to errors, its own scope check,
//! key sessions it signs and re-signs, login and server-sent events.
#![cfg(feature = "reqwest")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use hashinbox_client::client::RawEvent;
use hashinbox_client::models::{CreateWalletRequest, Kind};
use hashinbox_client::operations::*;
use hashinbox_client::{ApiError, Client, Error, Issue, Secret, Session, reason};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request as the server read it.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    head: String,
    body: String,
}

impl Seen {
    fn bearer(&self) -> Option<&str> {
        let line = self
            .head
            .lines()
            .find(|line| line.starts_with("authorization:"))?;
        line.strip_prefix("authorization: bearer ")
    }
}

/// An answer: status, content type, body.
type Reply = (u16, &'static str, String);

type Log = Arc<Mutex<Vec<Seen>>>;

/// A server on `127.0.0.1:0` answering the `n`th request it sees with
/// `reply(n, seen)`. Returns its URL and what it saw.
async fn server(reply: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static) -> (String, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log = Log::default();
    let reply = Arc::new(reply);
    let shared = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (log, reply) = (shared.clone(), reply.clone());
            tokio::spawn(async move {
                while let Some(request) = read(&mut stream).await {
                    let n = {
                        let mut log = log.lock().unwrap();
                        log.push(request.clone());
                        log.len() - 1
                    };
                    let (status, kind, body) = reply(n, &request);
                    let head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n\r\n",
                        body.len()
                    );
                    let wrote = stream.write_all(format!("{head}{body}").as_bytes()).await;
                    if wrote.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, log)
}

async fn read(stream: &mut TcpStream) -> Option<Seen> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        let n = stream.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
        bytes.extend_from_slice(&buffer[..n]);
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .map_or(0, |n| n.trim().parse().unwrap());
    let mut body = bytes[end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
        body.extend_from_slice(&buffer[..n]);
    }
    let raw = String::from_utf8_lossy(&bytes[..end]).into_owned();
    Some(Seen {
        line: raw.lines().next().unwrap().to_owned(),
        head,
        body: String::from_utf8(body).unwrap(),
    })
}

fn ok(body: Value) -> Reply {
    (200, "application/json", body.to_string())
}

fn error(status: u16, kind: &str, reason: Option<&str>) -> Reply {
    let body = json!({"kind": kind, "message": "no", "reason": reason, "metadata": {"scope": "s"}});
    (status, "application/json", body.to_string())
}

fn secret() -> Secret {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../_golden/session.json");
    let all: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    Secret::parse(all["id"].as_str().unwrap(), all["secret"].as_str().unwrap()).unwrap()
}

fn session(scope: &str) -> Session {
    secret().issue(Issue::new().scope(scope).unwrap()).unwrap()
}

fn wallet() -> Value {
    json!({"address": "0xabc", "chain": "ethereum", "external_id": null, "id": "ethereum.mainnet.0xabc",
           "indexes": {}, "keyed": true, "network": "mainnet", "tags": []})
}

fn session_read(scopes: Value) -> Value {
    json!({"amr": [], "expires_at": 0, "issued_at": 0, "kind": "key", "scopes": {}, "sub": null,
           "key": {"created_at": 0, "id": "hip/x", "name": "k", "scopes": scopes, "user_id": "u"},
           "user": {"created_at": 0, "email": "a@b", "id": "u", "login": true, "name": "a", "scopes": {}}})
}

#[tokio::test]
async fn it_sends_the_path_query_body_and_bearer() {
    let (url, log) = server(|n, _| match n {
        0 | 1 => ok(wallet()),
        _ => (204, "application/json", String::new()),
    })
    .await;
    let held = session("com.hashinbox");
    let client = Client::builder(url).session(held.clone()).build().unwrap();
    let id = "a/b c".to_owned();
    wallet_get(&client, WalletGetRequest { id }).await.unwrap();
    let body = CreateWalletRequest {
        address: None,
        chain: "ethereum".into(),
        external_id: Some("c-1".into()),
        network: "mainnet".into(),
        secret: None,
    };
    let created = wallets_create(&client, WalletsCreateRequest { body })
        .await
        .unwrap();
    assert_eq!(created.id, "ethereum.mainnet.0xabc");
    let all = Some(true);
    session_delete(&client, SessionDeleteRequest { all })
        .await
        .unwrap();
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen[0].line, "GET /v1/wallets/a%2Fb%20c HTTP/1.1");
    assert_eq!(seen[1].line, "POST /v1/wallets HTTP/1.1");
    assert!(seen[1].head.contains("content-type: application/json"));
    let sent: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(
        sent,
        json!({"chain": "ethereum", "network": "mainnet", "external_id": "c-1"})
    );
    assert_eq!(seen[2].line, "DELETE /v1/session?all=true HTTP/1.1");
    let token = held.token.to_lowercase();
    assert!(seen.iter().all(|seen| seen.bearer() == Some(&token)));
}

#[tokio::test]
async fn answers_map_to_errors() {
    let (url, _) = server(|n, _| match n {
        0 => error(403, "permission_denied", Some("missing_scope")),
        _ => (503, "text/plain", "down".into()),
    })
    .await;
    let client = Client::builder(url).build().unwrap();
    let Err(Error::Api(error)) = assets_list(&client, AssetsListRequest).await else {
        panic!("not an API error");
    };
    assert_eq!(
        (error.status, error.kind, error.message.as_str()),
        (403, Kind::PermissionDenied, "no")
    );
    assert_eq!(error.reason.as_deref(), Some("missing_scope"));
    assert_eq!(error.metadata["scope"], "s");
    let Err(Error::Api(error)) = health_get(&client, HealthGetRequest).await else {
        panic!("not an API error");
    };
    assert_eq!(
        (error.status, error.kind, error.message.as_str()),
        (
            503,
            Kind::Unavailable,
            "could not parse the response (HTTP 503)"
        )
    );
    assert_eq!(error.metadata["response"], "down");
}

#[tokio::test]
async fn a_missing_scope_fails_unsent() {
    let (url, log) = server(|_, _| ok(wallet())).await;
    let held = session("com.hashinbox.wallets[x]:read");
    let client = Client::builder(&url).session(held.clone()).build().unwrap();
    let request = WalletGetRequest { id: "y".into() };
    let Err(Error::Api(ApiError {
        status,
        kind,
        message,
        reason,
        metadata,
        ..
    })) = wallet_get(&client, request.clone()).await
    else {
        panic!("sent");
    };
    assert_eq!(
        (status, kind, reason.as_deref()),
        (403, Kind::PermissionDenied, Some(reason::MISSING_SCOPE))
    );
    assert_eq!(
        message,
        "wallet_get needs scope com.hashinbox.wallets[y]:read"
    );
    assert_eq!(metadata["scope"], "com.hashinbox.wallets[y]:read");
    assert_eq!(metadata["operation"], "wallet_get");
    assert!(log.lock().unwrap().is_empty());
    wallet_get(&client, WalletGetRequest { id: "x".into() })
        .await
        .unwrap();
    let unchecked = Client::builder(url)
        .session(held)
        .check_scopes(false)
        .build()
        .unwrap();
    wallet_get(&unchecked, request).await.unwrap();
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_secret_signs_its_sessions_with_the_keys_scopes_and_resigns_once() {
    let (url, log) = server(|n, _| match n {
        0 => ok(session_read(json!({"com.hashinbox": ["wallets"]}))),
        1 => ok(wallet()),
        2 | 4 => error(401, "unauthenticated", Some("session_expired")),
        3 => ok(session_read(json!({"com.hashinbox": ["wallets:read"]}))),
        _ => ok(wallet()),
    })
    .await;
    let client = Client::builder(url).secret(secret()).build().unwrap();
    let get = || wallet_get(&client, WalletGetRequest { id: "x".into() });
    get().await.unwrap();
    let first = client.session().unwrap();
    assert_eq!(first.issuer, secret().id());
    assert_eq!(
        first.scopes.to_string(),
        "com.hashinbox.jobs com.hashinbox.wallets"
    );
    let Err(Error::Api(refused)) = get().await else {
        panic!("not refused");
    };
    assert_eq!(refused.reason.as_deref(), Some("session_expired"));
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 5, "probe, call, refused, probe, retry refused");
    assert_eq!(seen[0].line, "GET /v1/session HTTP/1.1");
    let probe = seen[0].bearer().unwrap();
    assert!(!probe.starts_with("his/"));
    assert_eq!(seen[3].line, "GET /v1/session HTTP/1.1");
    assert!(
        seen.iter().all(|seen| !seen.head.contains("his/")),
        "never the secret"
    );
    assert_eq!(
        client.session().unwrap().scopes.to_string(),
        "com.hashinbox.jobs:read com.hashinbox.wallets:read"
    );
}

#[tokio::test]
async fn a_secrets_sessions_take_the_builders_scopes_and_are_kept() {
    let (url, log) = server(|_, _| ok(wallet())).await;
    let scopes = "com.hashinbox.wallets:read".parse().unwrap();
    let ttl = Duration::from_secs(300);
    let client = Client::builder(url)
        .secret(secret())
        .scopes(scopes)
        .ttl(ttl)
        .build()
        .unwrap();
    for _ in 0..2 {
        wallet_get(&client, WalletGetRequest { id: "x".into() })
            .await
            .unwrap();
    }
    let session = client.session().unwrap();
    assert_eq!(session.expires_at - session.issued_at, 300);
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "no probe");
    assert_eq!(seen[0].bearer(), seen[1].bearer(), "kept");
    let create = CreateWalletRequest {
        address: None,
        chain: "e".into(),
        external_id: None,
        network: "m".into(),
        secret: None,
    };
    let refused = wallets_create(&client, WalletsCreateRequest { body: create }).await;
    assert!(matches!(
        refused,
        Err(Error::Api(ApiError { status: 403, reason: Some(reason), .. })) if reason == "missing_scope"
    ));
    let long = Duration::from_secs(25 * 60 * 60);
    assert!(
        Client::builder("http://x")
            .secret(secret())
            .ttl(long)
            .build()
            .is_err()
    );
}

#[tokio::test]
async fn login_keeps_the_session_and_surfaces_its_reasons() {
    let token = session("com.hashinbox").token;
    let issued = json!({"token": token, "expires_at": 0, "scopes": {}});
    let (url, log) = server(move |n, _| match n {
        0 => error(401, "unauthenticated", Some("totp_required")),
        1 => error(401, "unauthenticated", Some("wrong_credentials")),
        2 => (201, "application/json", issued.to_string()),
        _ => (204, "application/json", String::new()),
    })
    .await;
    let client = Client::builder(url).build().unwrap();
    let refused = client.login("a@b", "pw", None).await.unwrap_err();
    assert_eq!(refused.reason(), Some("totp_required"));
    let refused = client.login("a@b", "pw", Some("000000")).await.unwrap_err();
    assert_eq!(refused.reason(), Some("wrong_credentials"));
    let session = client.login("a@b", "pw", Some("123456")).await.unwrap();
    assert_eq!(client.session(), Some(session.clone()));
    client.logout(true).await.unwrap();
    assert_eq!(client.session(), None);
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen[2].line, "POST /v1/session HTTP/1.1");
    assert_eq!(seen[2].bearer(), None);
    let sent: Value = serde_json::from_str(&seen[2].body).unwrap();
    assert_eq!(
        sent,
        json!({"email": "a@b", "password": "pw", "totp": "123456"})
    );
    assert_eq!(seen[3].line, "DELETE /v1/session?all=true HTTP/1.1");
    assert_eq!(
        seen[3].bearer(),
        Some(session.token.to_lowercase().as_str())
    );
}

#[tokio::test]
async fn a_stream_yields_its_events() {
    let text =
        "event: missed\ndata: {\"missed\":2}\n\n: alive\n\ndata: a\r\ndata: b\r\nid: 7\r\n\r\n";
    let (url, log) = server(move |_, _| (200, "text/event-stream", text.to_owned())).await;
    let client = Client::builder(url)
        .session(session("com.hashinbox"))
        .build()
        .unwrap();
    let request = LogsStreamRequest {
        level: Some("info".into()),
        ..Default::default()
    };
    let events: Vec<RawEvent> = client
        .stream(&request)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect()
        .await;
    let event = |event: &str, data: &str, id: Option<&str>| RawEvent {
        event: event.into(),
        data: data.into(),
        id: id.map(Into::into),
    };
    assert_eq!(
        events,
        [
            event("missed", "{\"missed\":2}", None),
            event("message", "a\nb", Some("7"))
        ]
    );
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen[0].line, "GET /v1/logs/stream?level=info HTTP/1.1");
    assert!(seen[0].head.contains("accept: text/event-stream"));
}

/// A client of `url` that may read logs.
fn streaming(url: String) -> Client {
    let client = Client::builder(url).session(session("com.hashinbox"));
    client.build().unwrap()
}

/// The query `seen` sent, if any.
fn query(seen: &Seen) -> Option<&str> {
    let target = seen.line.split(' ').nth(1)?;
    target.split_once('?').map(|(_, query)| query)
}

/// A window of `data` events, id each, ended by `close`.
fn window(events: &[(&str, &str)]) -> Reply {
    let mut text: String = events
        .iter()
        .map(|(id, data)| format!("data: {data}\nid: {id}\n\n"))
        .collect();
    text.push_str("event: close\ndata: {}\nretry: 0\n\n");
    (200, "text/event-stream", text)
}

#[tokio::test]
async fn a_stream_reads_on_across_windows_from_the_last_id() {
    let (url, log) = server(|n, _| match n {
        0 => window(&[("1", "a"), ("2", "b")]),
        _ => (200, "text/event-stream", "data: c\nid: 3\n\n".to_owned()),
    })
    .await;
    let client = streaming(url);
    let request = LogsStreamRequest {
        level: Some("info".into()),
        cursor: Some("0".into()),
        ..Default::default()
    };
    let mut events = client.stream(&request).await.unwrap();
    let mut read = Vec::new();
    while let Some(event) = events.next().await {
        read.push(event.unwrap().data);
    }
    assert_eq!(read, ["a", "b", "c"], "each once, in order; no close");
    assert_eq!(events.cursor(), Some("3"));
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "ended without close: done");
    assert_eq!(query(&seen[0]), Some("level=info&cursor=0"));
    assert_eq!(query(&seen[1]), Some("level=info&cursor=2"), "overridden");
}

#[tokio::test]
async fn a_stopped_stream_reconnects_no_more() {
    let (url, log) = server(|n, _| window(&[(&n.to_string(), "x")])).await;
    let client = streaming(url);
    let mut events = client.stream(&LogsStreamRequest::default()).await.unwrap();
    for _ in 0..3 {
        events.next().await.unwrap().unwrap();
    }
    assert_eq!(events.cursor(), Some("2"));
    events.stop();
    assert!(events.next().await.is_none());
    let opened = log.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(log.lock().unwrap().len(), opened, "none opened after stop");
}

#[tokio::test]
async fn a_stream_backs_off_after_an_error_and_ends_on_a_refusal() {
    let (url, log) = server(|n, _| match n {
        0 => window(&[("1", "a")]),
        1 | 2 => error(503, "unavailable", None),
        3 => window(&[("2", "b")]),
        _ => error(401, "unauthenticated", Some("session_expired")),
    })
    .await;
    let client = streaming(url);
    let events = client.stream(&LogsStreamRequest::default()).await.unwrap();
    let mut events = events.backoff(Duration::from_millis(20), Duration::from_millis(30));
    let started = std::time::Instant::now();
    assert_eq!(events.next().await.unwrap().unwrap().data, "a");
    assert_eq!(events.next().await.unwrap().unwrap().data, "b");
    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "20 then 30 ms"
    );
    let refused = events.next().await.unwrap().unwrap_err();
    assert!(
        matches!(refused, Error::Api(ApiError { status: 401, .. })),
        "{refused}"
    );
    assert!(events.next().await.is_none());
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 5);
    let resumed: Vec<_> = seen.iter().map(query).collect();
    let cursor = Some("cursor=1");
    assert_eq!(resumed, [None, cursor, cursor, cursor, Some("cursor=2")]);
}

#[test]
fn any_error_is_an_api_error() {
    type Failed = Error<std::io::Error>;
    let refused = ApiError::from(Failed::Api(ApiError::from(
        hashinbox_client::client::Response {
            status: 401,
            body: json!({"kind": "unauthenticated", "message": "no", "reason": "totp_required"})
                .to_string(),
        },
    )));
    assert_eq!(refused.reason.as_deref(), Some(reason::TOTP_REQUIRED));
    assert_eq!(
        (refused.status, refused.kind, refused.message.as_str()),
        (401, Kind::Unauthenticated, "no")
    );
    assert!(refused.cause.is_none());

    let down = ApiError::from(Failed::Transport(std::io::ErrorKind::TimedOut.into()));
    assert_eq!(
        (down.status, down.kind, down.reason),
        (0, Kind::Unavailable, None)
    );
    let cause = down.cause.unwrap();
    assert!(matches!(cause.downcast_ref(), Some(Failed::Transport(_))));
    assert_eq!(down.message, cause.to_string());

    let garbled = ApiError::from(Failed::Decode(serde_json::from_str::<u8>("x").unwrap_err()));
    assert_eq!(
        (garbled.status, garbled.kind, garbled.reason),
        (0, Kind::Unavailable, None)
    );
    assert!(matches!(
        garbled.cause.unwrap().downcast_ref(),
        Some(Failed::Decode(_))
    ));

    let made = ApiError {
        message: "made".into(),
        ..ApiError::default()
    };
    assert_eq!(
        (made.status, made.kind, made.reason, made.metadata.len()),
        (0, Kind::Unavailable, None, 0)
    );
}

#[test]
fn an_answer_not_the_apis_error_is_unavailable() {
    let read = |status, body: &str| {
        let body = body.to_owned();
        ApiError::from(hashinbox_client::client::Response { status, body })
    };
    let html = format!("<html>{}</html>", "x".repeat(2000));
    for (status, body) in [
        (502, html.as_str()),
        (503, ""),
        (503, r#"{"status":"down"}"#),
        (500, r#"{"kind":"strange","message":"no"}"#),
    ] {
        let error = read(status, body);
        assert_eq!(
            (error.status, error.kind, error.reason, error.message),
            (
                status,
                Kind::Unavailable,
                None,
                format!("could not parse the response (HTTP {status})")
            )
        );
        let response: String = body.chars().take(1024).collect();
        assert_eq!(error.metadata, [("response".to_owned(), response)].into());
    }
    let error = read(
        403,
        r#"{"kind":"permission_denied","message":"no","reason":"missing_scope","metadata":{"scope":"s"}}"#,
    );
    assert_eq!(
        (error.status, error.kind, error.message.as_str()),
        (403, Kind::PermissionDenied, "no")
    );
    assert_eq!(error.reason.as_deref(), Some(reason::MISSING_SCOPE));
    assert_eq!(error.metadata["scope"], "s");
}
