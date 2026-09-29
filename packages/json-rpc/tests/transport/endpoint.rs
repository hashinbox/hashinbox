//! One node's URL, headers and bounds: what reaches the node unaltered, what
//! is rejected, and what a `${NAME}` reference does and does not disclose.

use std::collections::BTreeMap;

use json_rpc::{Bounds, Client, Endpoint, ErrorKind, Request};
use serde_json::json;

use crate::node;

fn secrets<const N: usize>(values: [(&str, &str); N]) -> BTreeMap<String, String> {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

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

/// A `${NAME}` may stand anywhere in the URL, which is how every provider
/// that puts its key in the path or the query is configured.
#[tokio::test]
async fn a_reference_resolves_anywhere_in_the_url() {
    let (address, observed) = node::serve_target();
    Endpoint::new(format!("{address}/v3/${{RPC_KEY}}?apikey=${{RPC_KEY}}"))
        .connect(&secrets([("RPC_KEY", "k3y")]))
        .expect("the named variable is set")
        .request(Request::once("answer", json!([])))
        .await
        .expect("one execution")
        .expect("call must succeed");
    assert_eq!(
        observed.recv().expect("the server must report its target"),
        "/v3/k3y?apikey=k3y"
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
    Endpoint::new("http://${RPC_HOST}/v1?apikey=${RPC_KEY}")
        .validate()
        .expect("a reference stands for one word wherever it is written");
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
fn an_invalid_header_name_is_rejected_without_echoing_its_value() {
    let error = Endpoint::new("http://node.invalid/")
        .with_header("bad header", "s3cret")
        .validate()
        .expect_err("a header name is HTTP's, not free text");
    assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
    assert_eq!(error.message, "JSON-RPC header name is invalid");
    assert!(!error.message.contains("s3cret"));
}

/// HTTP compares header names case-insensitively, so two spellings of one
/// name would silently leave whichever the map ordered last.
#[test]
fn header_names_that_collide_as_http_compares_them_are_rejected() {
    let error = Endpoint::new("http://node.invalid/")
        .with_header("X-API-Key", "first")
        .with_header("x-api-key", "second")
        .validate()
        .expect_err("one name, two values");
    assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
    assert_eq!(
        error.message,
        "JSON-RPC endpoint header names collide case-insensitively"
    );
    Endpoint::new("http://node.invalid/")
        .with_header("X-API-Key", "a")
        .with_header("X-Other", "b")
        .validate()
        .expect("two different names");
}

#[test]
fn a_malformed_environment_reference_is_rejected_before_any_lookup() {
    for reference in [
        "${lowercase}",
        "${9START}",
        "${WITH-DASH}",
        "${UNCLOSED",
        "${}",
    ] {
        for endpoint in [
            Endpoint::new(format!("http://node.invalid/{reference}")),
            Endpoint::new("http://node.invalid/").with_header("x-key", reference),
        ] {
            let error = endpoint
                .validate()
                .expect_err("a reference must be a usable variable name");
            assert_eq!(
                error.message,
                "JSON-RPC endpoint environment reference is malformed"
            );
        }
    }
    Endpoint::new("http://node.invalid/")
        .with_header("x-key", "prefix-${A_1}-${B}")
        .validate()
        .expect("two references in one value");
}

#[test]
fn an_unresolved_reference_discloses_neither_the_variable_nor_the_endpoint() {
    let endpoint =
        Endpoint::new("https://eth.provider.invalid/v1").with_header("X-API-Key", "${ETH_RPC_KEY}");
    let error = endpoint
        .connect(&secrets([("OTHER", "value")]))
        .expect_err("the named variable is not set");
    assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
    assert_eq!(
        error.message,
        "a configured endpoint's environment variable is unavailable"
    );
    assert!(!error.to_string().contains("ETH_RPC_KEY"));
    assert!(!error.to_string().contains("eth.provider.invalid"));
    assert!(!format!("{endpoint:?}").contains("eth.provider.invalid"));
}

#[test]
fn a_value_that_would_be_percent_encoded_in_the_url_is_rejected() {
    for (url, values) in [
        (
            "http://user:${BTC_RPC_PASSWORD}@node.invalid/",
            [("BTC_RPC_PASSWORD", "p@ss/word")],
        ),
        (
            "http://${BTC_RPC_USER}:pass@node.invalid/",
            [("BTC_RPC_USER", "user name")],
        ),
        (
            "https://node.invalid/v3/${RPC_KEY}",
            [("RPC_KEY", "key with space")],
        ),
    ] {
        let error = Endpoint::new(url)
            .connect(&secrets(values))
            .expect_err("the HTTP client would send the encoded text");
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        assert_eq!(
            error.message,
            "JSON-RPC endpoint value must not require percent-encoding"
        );
    }
    Endpoint::new("http://user:simple-pass@node.invalid/")
        .connect(&BTreeMap::new())
        .expect("a simple credential is supported");
}

#[test]
fn an_endpoint_is_written_as_a_url_or_as_an_object() {
    let bare = serde_json::from_value::<Endpoint>(json!("https://node.invalid/v1?apikey=k3y"))
        .expect("a node URL alone");
    assert_eq!(bare, Endpoint::new("https://node.invalid/v1?apikey=k3y"));
    assert_eq!(bare.bounds, Bounds::default());

    let written = serde_json::from_value::<Endpoint>(json!({
        "url": "https://node.invalid/v1",
        "headers": {"X-API-Key": "${RPC_KEY}"},
        "bounds": {"timeout_ms": 20000},
    }))
    .expect("a node with headers and bounds");
    assert_eq!(
        written,
        Endpoint::new("https://node.invalid/v1")
            .with_header("X-API-Key", "${RPC_KEY}")
            .with_bounds(Bounds {
                timeout_ms: 20_000,
                ..Bounds::default()
            })
    );

    for (value, expected) in [
        (json!({"headers": {}}), "missing field `url`"),
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
async fn every_header_reaches_the_node_with_its_value_resolved() {
    let (address, observed) = node::serve_headers();
    Endpoint::new(address)
        .with_header("x-tenant", "acme")
        .with_header("authorization", "Basic ${BTC_RPC_BASIC}")
        .connect(&secrets([("BTC_RPC_BASIC", "dXNlcjpwYXNz+w==")]))
        .expect("the named variable is set")
        .request(Request::once("answer", json!([])))
        .await
        .expect("one execution")
        .expect("call must succeed");
    let headers = observed.recv().expect("the server must report its headers");
    for expected in [
        ("x-tenant", "acme"),
        ("authorization", "Basic dXNlcjpwYXNz+w=="),
    ] {
        assert!(
            headers.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "{headers:?}"
        );
    }
}

#[tokio::test]
async fn userinfo_becomes_http_basic() {
    let (address, observed) = node::serve_headers();
    let authority = address.strip_prefix("http://").expect("loopback address");
    Endpoint::new(format!(
        "http://${{BTC_RPC_USER}}:${{BTC_RPC_PASSWORD}}@{authority}/"
    ))
    .connect(&secrets([
        ("BTC_RPC_USER", "user"),
        ("BTC_RPC_PASSWORD", "pass"),
    ]))
    .expect("both variables are set")
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
