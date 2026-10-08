//! One node's URL and bounds: what reaches the node unaltered, what is
//! rejected, and what is shown.

use json_rpc::{Bounds, Client, Endpoint, ErrorKind, Request};
use serde_json::json;

use crate::node;

/// The defect this shape fixes: a provider that carries its API key in the
/// path or the query could not be configured at all, because the query string
/// was read as this crate's own parameters and then dropped.
#[tokio::test]
async fn a_providers_own_query_string_reaches_the_node_unaltered() {
    let (address, observed) = node::serve_target();
    node::http(&format!("{address}/v3/project?apikey=s3cret&auth=token"))
        .request(Request::once("answer", json!([])))
        .await
        .expect("one execution")
        .expect("call must succeed");
    assert_eq!(
        observed.recv().expect("the server must report its target"),
        "/v3/project?apikey=s3cret&auth=token"
    );
}

#[test]
fn the_url_shape_itself_is_validated() {
    for (url, expected) in [
        (
            "ws://node.invalid/",
            "JSON-RPC endpoint scheme must be http or https",
        ),
        (
            "ftp://node.invalid/",
            "JSON-RPC endpoint scheme must be http or https",
        ),
        (
            "file:///tmp/node",
            "JSON-RPC endpoint scheme must be http or https",
        ),
        (
            "http://node.invalid/#section",
            "JSON-RPC endpoint must not carry a fragment",
        ),
        ("not a url", "JSON-RPC endpoint URL is invalid"),
    ] {
        let error = Endpoint::new(url)
            .validate()
            .expect_err("the URL shape must be checked");
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        assert_eq!(error.message, expected);
    }
}

#[test]
fn a_bound_must_be_a_positive_integer() {
    for (bounds, name) in [
        (
            Bounds {
                timeout_ms: 0,
                ..Bounds::default()
            },
            "timeout_ms",
        ),
        (
            Bounds {
                max_request_bytes: 0,
                ..Bounds::default()
            },
            "max_request_bytes",
        ),
        (
            Bounds {
                max_response_bytes: 0,
                ..Bounds::default()
            },
            "max_response_bytes",
        ),
    ] {
        let error = Endpoint::new("http://node.invalid/")
            .with_bounds(bounds)
            .validate()
            .expect_err("a bound must be usable");
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        assert_eq!(
            error.message,
            format!("JSON-RPC endpoint `{name}` must be a positive integer")
        );
    }
    assert_eq!(
        Bounds::default(),
        Bounds {
            timeout_ms: 15_000,
            max_request_bytes: 10 * 1024 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
        }
    );
}

#[test]
fn an_endpoint_is_written_as_a_url_or_as_an_object() {
    let bare = serde_json::from_value::<Endpoint>(json!("https://node.invalid/v1?apikey=k3y"))
        .expect("a node URL alone");
    assert_eq!(bare, Endpoint::new("https://node.invalid/v1?apikey=k3y"));
    assert_eq!(bare.bounds, Bounds::default());

    let written = serde_json::from_value::<Endpoint>(json!({
        "url": "https://node.invalid/v1",
        "bounds": {"timeout_ms": 20000},
    }))
    .expect("a node with bounds");
    assert_eq!(
        written,
        Endpoint::new("https://node.invalid/v1").with_bounds(Bounds {
            timeout_ms: 20_000,
            ..Bounds::default()
        })
    );

    for (value, expected) in [
        (json!({"bounds": {}}), "missing field `url`"),
        (
            json!({"url": "http://a", "headers": {}}),
            "unknown field `headers`",
        ),
        (
            json!({"url": "http://a", "timeout_ms": 1}),
            "unknown field `timeout_ms`",
        ),
        (
            json!({"url": "http://a", "bounds": {"timeout": 1}}),
            "unknown field `timeout`",
        ),
        (json!(7), "invalid type: integer `7`"),
    ] {
        let error = serde_json::from_value::<Endpoint>(value).expect_err("a strict node schema");
        assert!(error.to_string().starts_with(expected), "{error}");
    }
}

#[tokio::test]
async fn userinfo_becomes_http_basic() {
    let (address, observed) = node::serve_headers();
    let authority = address.strip_prefix("http://").expect("loopback address");
    Endpoint::new(format!("http://user:pass@{authority}/"))
        .connect()
        .expect("a URL with userinfo")
        .request(Request::once("answer", json!([])))
        .await
        .expect("one execution")
        .expect("call must succeed");
    let headers = observed.recv().expect("headers must be observed");
    assert!(
        headers.contains(&(
            "authorization".to_owned(),
            format!("Basic {}", "dXNlcjpwYXNz")
        )),
        "{headers:?}"
    );
}

#[test]
fn shown_form_redacts_userinfo_and_query_values() {
    for (url, shown) in [
        (
            "http://user:s3cret@127.0.0.1:18443/wallet?apikey=s3cret&x=1",
            "http://***@127.0.0.1:18443/wallet?apikey=***&x=***",
        ),
        ("https://node.invalid/v1", "https://node.invalid/v1"),
        ("not a url s3cret", "invalid URL"),
    ] {
        let endpoint = Endpoint::new(url);
        assert_eq!(endpoint.redacted(), shown);
        assert_eq!(endpoint.to_string(), shown);
        assert!(!format!("{endpoint:?}").contains("s3cret"));
    }
}
