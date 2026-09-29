//! Loopback JSON-RPC servers the transport tests run against.

use std::{
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use json_rpc::{Endpoint, Http};
use serde_json::{Value, json};

/// One `Http` over `endpoint`, with no retry, no failover, no added header and
/// the default bounds.
pub fn http(endpoint: &str) -> Http {
    Endpoint::new(endpoint)
        .connect(&std::collections::BTreeMap::new())
        .expect("loopback client must build")
}

/// A bound but never accepted address: every connection to it fails.
pub fn unreachable() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral address must bind");
    format!(
        "http://{}",
        listener.local_addr().expect("address must exist")
    )
}

/// A listener that observes connection attempts without serving them.
pub fn silent() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    listener
        .set_nonblocking(true)
        .expect("request observation must not block");
    let address = listener.local_addr().expect("loopback address must exist");
    (listener, format!("http://{address}"))
}

/// Asserts that nothing has connected to `listener`.
pub fn untouched(listener: &std::net::TcpListener) {
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

/// Answers one request with `response` applied to the decoded request body.
pub fn serve_once<T: serde::Serialize>(
    response: impl FnOnce(Value) -> T + Send + 'static,
) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let request = read_request(&mut stream);
        write_json_response(&mut stream, &response(request)).expect("response must write");
    });
    format!("http://{address}")
}

/// Answers one request with an HTTP status line and counts the execution.
pub fn serve_status_once(executions: Arc<AtomicUsize>, status: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let _ = read_request(&mut stream);
        executions.fetch_add(1, Ordering::SeqCst);
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .expect("status response must write");
    });
    format!("http://{address}")
}

/// Rejects `statuses.len()` single requests in order, reporting each one under
/// `name` so a test can assert the exact visiting order.
pub fn serve_request_rejections<const N: usize>(
    name: &'static str,
    statuses: [&'static str; N],
    events: mpsc::Sender<&'static str>,
) -> String {
    serve_rejections(name, statuses, events, false)
}

/// The batch counterpart of [`serve_request_rejections`].
pub fn serve_batch_rejections<const N: usize>(
    name: &'static str,
    statuses: [&'static str; N],
    events: mpsc::Sender<&'static str>,
) -> String {
    serve_rejections(name, statuses, events, true)
}

fn serve_rejections<const N: usize>(
    name: &'static str,
    statuses: [&'static str; N],
    events: mpsc::Sender<&'static str>,
    batch: bool,
) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    thread::spawn(move || {
        for status in statuses {
            let (mut stream, _) = listener.accept().expect("client must connect");
            let request = read_request(&mut stream);
            let call = if batch {
                let calls = request.as_array().expect("batch request must be an array");
                assert_eq!(calls.len(), 1);
                calls[0].clone()
            } else {
                request
            };
            assert_eq!(call["method"], "answer");
            assert_eq!(call["params"], json!([42]));
            events.send(name).expect("attempt must be observed");
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("status response must write");
        }
    });
    format!("http://{address}")
}

/// Answers one request with `42` and reports the request headers it saw, so a
/// test can assert exactly what a resolved endpoint puts on the wire.
pub fn serve_headers() -> (String, mpsc::Receiver<Vec<(String, String)>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    let (send, observed) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let (_, headers, request) = read(&mut stream);
        send.send(headers).expect("headers must be observed");
        let response = json!({"jsonrpc":"2.0", "id":request["id"], "result":42});
        write_json_response(&mut stream, &response).expect("response must write");
    });
    (format!("http://{address}"), observed)
}

/// Answers one request with `42` and reports the request target it saw, so a
/// test can assert that a provider's own URL reaches the node unaltered.
pub fn serve_target() -> (String, mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    let (send, observed) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let (target, _, request) = read(&mut stream);
        send.send(target).expect("target must be observed");
        let response = json!({"jsonrpc":"2.0", "id":request["id"], "result":42});
        write_json_response(&mut stream, &response).expect("response must write");
    });
    (format!("http://{address}"), observed)
}

/// A server that holds its answer until it is released.
pub struct Held {
    pub endpoint: String,
    pub observed: mpsc::Receiver<Value>,
    pub release: mpsc::Sender<()>,
    pub thread: thread::JoinHandle<()>,
}

/// Observes one request, then waits to be released before answering.
pub fn serve_held() -> Held {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    let (observed_send, observed) = mpsc::channel();
    let (release, release_receive) = mpsc::channel();
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let request = read_request(&mut stream);
        observed_send
            .send(request.clone())
            .expect("request observation must send");
        release_receive
            .recv_timeout(Duration::from_secs(2))
            .expect("held response must be released");
        let response = json!({"jsonrpc":"2.0", "id":request["id"], "result":"released"});
        let _ = write_json_response(&mut stream, &response);
    });
    Held {
        endpoint: format!("http://{address}"),
        observed,
        release,
        thread,
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Value {
    read(stream).2
}

/// The request's target (path and query), its headers lowercased, and its
/// decoded JSON body.
fn read(stream: &mut std::net::TcpStream) -> (String, Vec<(String, String)>, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout must configure");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("request must read");
        assert_ne!(count, 0, "request body ended before it was complete");
        bytes.extend_from_slice(&chunk[..count]);
        let Some(split) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
            continue;
        };
        let header = String::from_utf8_lossy(&bytes[..split]);
        let target = header
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("request line must name a target")
            .to_owned();
        let headers = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<Vec<_>>();
        let length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .expect("request must declare content length");
        let body_start = split + 4;
        if bytes.len() < body_start + length {
            continue;
        }
        let body = serde_json::from_slice(&bytes[body_start..body_start + length])
            .expect("request body must be JSON");
        return (target, headers, body);
    }
}

fn write_json_response(
    stream: &mut std::net::TcpStream,
    response: &impl serde::Serialize,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(response).expect("response must encode");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
        Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}
