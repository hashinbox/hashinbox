# rpc

A JSON-RPC client over HTTP, built from small decorators that are each a
`Client`. Callers are written against the `Client` trait, so a test can
substitute a scripted node.

| Decorator | One job |
|---|---|
| `Http` | JSON-RPC over HTTP to one node; answers mapped to results, error objects and `Error` kinds |
| `Split` | sends a batch in requests of at most `rpc.max_batch` calls |
| `Throttle` | paces requests to at most `rpc.rps` calls a second; waits, never fails |
| `Logged` | logs a node's failure streaks and recovery to an injected `logging::Logger` |
| `Counted` | counts a node's outcomes into `Statistics` |
| `Retry` | repeats retryable work with backoff |
| `Swap` | replaces the stack live |

Decorators keep no selection state. Choosing among a network's nodes
(tips, failure streaks, routed methods) is the application's: the server's
`usecase::Transport` is one `Client` over its nodes, each built here.
`Client::endpoint()` is the one node behind a client (`Http` answers it,
every decorator forwards it; `None` over several), and `Endpoint::id()` is
a node's identity: `host[:port]/path` and the query sent, without scheme,
userinfo, `header.*` or `rpc.*` params.

## Usage

`rpc::node` turns one URL into that node's preset,
`Node = Counted<Logged<Split<Throttle<Http>>>>`. It does no I/O.

```rust
use rpc::{Client, Endpoint, Request, Retry};
use serde_json::json;

async fn version(rpc: &dyn Client) -> Result<String, rpc::Error> {
    let raw = rpc.request(Request::new("version", json!([]))).await?;
    raw.deserialize()
}

fn connect() -> Result<impl Client, rpc::Error> {
    // The URL carries everything: userinfo becomes HTTP Basic, query keys stay,
    // a `header.<Name>` parameter is sent as that header, and an `rpc.`
    // parameter is a setting that never reaches the node.
    let url = "https://user:pass@node.invalid/?header.x-api-key=KEY&rpc.timeout_ms=5000";
    let node = rpc::node(&Endpoint::new(url), logging::Logger::default())?;
    // Three attempts, 250 ms doubling to 2 s.
    Ok(Retry::new(node))
}
# connect().expect("the endpoint is valid");
```

## Errors

A call ends one of three ways, all in `src/error.rs`:

- a result, `RawJson`;
- `Error::Remote(Failure)`: the node answered with a JSON-RPC error object;
- `Error::Transport { kind, message }`: no answer to interpret. An HTTP
  status is a transport kind, `ErrorKind::HttpStatus`, beside timeouts,
  unreachable nodes, oversized or invalid bodies, and requests refused
  before sending.

`request` and `broadcast` return `Result<_, Error>`. A batch returns one
`Answer` (`Result<RawJson, Failure>`) per call, since one call can fail
alone; a rate limit on any call, or a transport error, fails the whole batch.

One policy, used by `Retry` and the server's transport:
`Error::is_rate_limited` is HTTP 429, or an error object
`Failure::is_rate_limited` says is one (`-32005`, or `-32003` naming a rate
or request limit). `Error::is_retryable` is that, a timeout, an unreachable
node, or HTTP 502/503/504. Any other error object is the node's answer and
is not repeated.

## The URL

| Written in the URL | Meaning | Default |
|---|---|---|
| scheme, host, port, path, query | sent as written (provider keys) | – |
| `user:password@` | HTTP Basic | – |
| `header.<Name>=v` | static header, removed from the URL | – |
| `rpc.timeout_ms=N` | one execution's limit | 15000 |
| `rpc.max_request_bytes=N` | request body limit | 10 MiB |
| `rpc.max_response_bytes=N` | response body limit | 64 MiB |
| `rpc.max_batch=N` | most calls per request; 1 sends no batches | none |
| `rpc.rps=X` | most calls a second (`3`, `3.5`); a batch counts per call; a request waits for its turn | none |

An `Endpoint` is the URL and serializes as it. `Endpoint::parse` checks it
and gives an `rpc::Url`: `node`, the URL as sent, and `params`, every query
parameter by name (`get`, `get_int`, and `namespace("rpc")` for those under
one prefix). Each setting is read from it by its owner:
`HttpParams` in `http.rs`, `SplitParams` in `decorators/split.rs`,
`ThrottleParams` in `decorators/throttle.rs`, each `TryFrom<&Url>`.
`Endpoint::validate` builds the node, which does no I/O. Retry, routes and
the logger never go in a URL.

## Swap

`Swap` is a `Client` whose nodes are replaced while every holder keeps the
same handle: compose a caller over it once, then `Swap::replace` installs
another transport together with its nodes' `Statistics`. Each call finishes on
the transport it started on; later calls reach the replacement. Batches and
broadcasts are forwarded, so a swapped fan-out still broadcasts.

```rust
use std::sync::Arc;

use rpc::{Client, Endpoint, Swap};

fn repoint() -> Result<(), rpc::Error> {
    let first = rpc::node(&Endpoint::new("http://127.0.0.1:8545"), Default::default())?;
    let counters = vec![first.statistics()];
    let swap = Arc::new(Swap::new(Arc::new(first), counters));
    let held: Arc<dyn Client> = swap.clone(); // what a caller keeps

    let second = rpc::node(&Endpoint::new("http://127.0.0.1:8546"), Default::default())?;
    let counters = vec![second.statistics()];
    swap.replace(Arc::new(second), counters);
    drop(held);
    Ok(())
}
# repoint().expect("valid endpoints");
```

## Notes

- `Counted::statistics()` is a cloneable handle that samples attempts,
  failures, consecutive failures and a sanitized last failure from memory
  without locks or RPC. One logical call, batch or broadcast counts once,
  however `Split` sends it; any transport or JSON-RPC error makes it a
  failure. Successful completions reset the consecutive counter. Work refused
  before sending (an invalid parameter) does not count; cancellation records
  no invented outcome. Label handles by configured ordinal, never by a
  credential-bearing URL.
- Submit a transaction with `Request::once` or `Batch::once`. `Retry` (and
  any selection over nodes) then executes it exactly once against one node, because a repeated
  submission can send the same payment twice. Pass it to `Client::broadcast`
  to reach every node of a fan-out client once; every decorator forwards a
  broadcast, so wrapping never narrows it to one node. When no node
  succeeds, a transport failure is returned before an error object, because
  that node may have accepted the envelope.
- `Logged` reports sanitized failures, repeated-failure counts (at most every
  30 seconds), and recovery to a logger scoped to the node. No URL, request
  parameters or provider error prose reaches the sink.

The `testing` feature adds `rpc::testing::Script`, a node double that
replies in order and asserts each method and its parameters, and `Faults`,
which makes any client fail on demand. Enable it in `[dev-dependencies]` only.
