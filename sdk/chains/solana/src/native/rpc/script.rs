//! Solana's half of the shared scripted node (`json_rpc::testing`): the
//! `localnet` cluster its client claims, and the exchange shape this chain
//! scripts — a method, its exact parameters, and its result, so a Solana
//! fixture cannot omit the parameter assertion. The fixtures themselves stay
//! in the tests that own them.

pub(crate) use json_rpc::testing::Script;

use json_rpc::testing::Reply;
use serde_json::Value;

use super::Client;
use crate::native::Network;

/// Genesis hash of every scripted cluster.
pub(crate) const GENESIS: &str = "11111111111111111111111111111111";

/// What a Solana script adds to the shared double.
pub(crate) trait Scripted: Sized {
    /// The `localnet` cluster every scripted client claims.
    fn network() -> Network;

    /// A script of exact exchanges: `(method, parameters, result)`, in order.
    fn exchanges(values: impl IntoIterator<Item = (&'static str, Value, Value)>) -> Self;

    /// A client over this script that skips connection checks.
    fn client(&self) -> Client<Script>;
}

impl Scripted for Script {
    fn network() -> Network {
        Network::new("localnet", GENESIS.parse().expect("canonical genesis"))
    }

    fn exchanges(values: impl IntoIterator<Item = (&'static str, Value, Value)>) -> Self {
        let replies = values
            .into_iter()
            .map(|(method, params, result)| Reply::ok(method, result).expecting(params));
        Self::new(replies)
    }

    fn client(&self) -> Client<Script> {
        Client::unverified(self.clone(), Self::network())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use serde_json::json;

    use std::collections::BTreeMap;

    use json_rpc::{Bounds, Endpoint, Http};

    use super::*;

    /// One node, under the bounds this test needs.
    fn http(endpoint: &str, max_response_bytes: u32) -> Client<Http> {
        let transport = Endpoint::new(endpoint)
            .with_bounds(Bounds {
                timeout_ms: 2_000,
                max_request_bytes: 1024,
                max_response_bytes,
            })
            .connect(&BTreeMap::new())
            .expect("loopback transport");
        Client::unverified(transport, Script::network())
    }

    #[tokio::test]
    async fn loopback_asserts_exact_wire_call_endpoint_and_response() {
        let endpoint = serve_once(|request| {
            assert_eq!(request["method"], "getHealth");
            assert_eq!(request["params"], json!([]));
            json!({"jsonrpc":"2.0","id":request["id"],"result":"ok"})
        });
        http(&endpoint, 1_024)
            .health()
            .await
            .expect("exact response");
    }

    #[tokio::test]
    async fn loopback_enforces_response_limit_after_one_call() {
        let endpoint = serve_once(
            |request| json!({"jsonrpc":"2.0","id":request["id"],"result":"x".repeat(512)}),
        );
        let error = http(&endpoint, 64).health().await.unwrap_err();
        assert_eq!(error.message, "Solana RPC request failed: ResponseTooLarge");
    }

    #[tokio::test]
    async fn loopback_request_is_cancelled_without_a_second_call() {
        let Held {
            endpoint,
            observed,
            release,
            thread,
        } = serve_held();
        let client = http(&endpoint, 1_024);
        let request = tokio::spawn(async move { client.health().await });
        let observed = tokio::task::spawn_blocking(move || {
            observed.recv_timeout(Duration::from_secs(2)).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(observed["method"], "getHealth");
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
    }

    fn serve_once(response: impl FnOnce(Value) -> Value + Send + 'static) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let address = listener.local_addr().expect("loopback address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request");
            let request = read_request(&mut stream);
            write_response(&mut stream, &response(request));
        });
        format!("http://{address}")
    }

    struct Held {
        endpoint: String,
        observed: mpsc::Receiver<Value>,
        release: mpsc::Sender<()>,
        thread: thread::JoinHandle<()>,
    }

    fn serve_held() -> Held {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let address = listener.local_addr().expect("loopback address");
        let (observed_send, observed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request");
            let request = read_request(&mut stream);
            observed_send.send(request).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            // Release follows confirmed cancellation; close without replying to the
            // abandoned request.
            drop(stream);
        });
        Held {
            endpoint: format!("http://{address}"),
            observed,
            release,
            thread,
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Value {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 2_048];
        loop {
            let count = stream.read(&mut chunk).expect("request bytes");
            assert_ne!(count, 0);
            bytes.extend_from_slice(&chunk[..count]);
            let Some(split) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&bytes[..split]).unwrap();
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                .expect("content length");
            let start = split + 4;
            if bytes.len() >= start + length {
                return serde_json::from_slice(&bytes[start..start + length]).unwrap();
            }
        }
    }

    fn write_response(stream: &mut std::net::TcpStream, value: &Value) {
        let body = serde_json::to_vec(value).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    }
}
