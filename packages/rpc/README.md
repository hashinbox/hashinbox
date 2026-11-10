# rpc

A JSON-RPC client over HTTP with timeouts, size limits, retry and failover.
Chain code is written against the `Client` trait, so a test can substitute a
scripted node.

## Usage

```rust
use rpc::{CallError, Client, Endpoint, Failover, Request, Retry};
use serde_json::json;

async fn block_count(rpc: &dyn Client) -> Result<u64, CallError> {
    let raw = rpc.request(Request::new("getblockcount", json!([]))).await??;
    Ok(raw.deserialize()?)
}

fn connect() -> Result<impl Client, rpc::Error> {
    // The URL carries everything: userinfo becomes HTTP Basic, query keys stay,
    // and a `header.<Name>` parameter is sent as that header instead.
    let primary = Endpoint::new("https://user:pass@node.invalid/").connect()?;
    let backup = Endpoint::new("https://backup.invalid/v1?header.x-api-key=KEY").connect()?;

    // Three attempts; one attempt tries each endpoint in order.
    Ok(Retry::new(Failover::new(vec![primary, backup])?))
}
# connect().expect("endpoints are valid");
```

## Cluster

`Cluster` makes several nodes of one network act as one `Client`. Requests go
to the primary: the node with the highest tip, read by a probe the chain
injects. Building asks no node; the first call probes. A failing or rate-limited primary, or an elapsed refresh interval,
makes the next call probe again; a tie keeps the incumbent, and a node more
than `Policy::max_lag` behind never takes over. `Client::broadcast` sends one
request to every node exactly once and returns the first success.
`Cluster::one` wraps a single node.

```rust
use rpc::{BoxFuture, CallError, Client, Cluster, Http, Policy, Request};
use serde_json::json;

/// The chain's tip reader: Bitcoin Core's block count.
fn tip(node: &Http) -> BoxFuture<'_, Result<u64, CallError>> {
    Box::pin(async move {
        let raw = node.request(Request::new("getblockcount", json!([]))).await??;
        Ok(raw.deserialize()?)
    })
}

async fn submit(nodes: Vec<Http>, signed: String) -> Result<(), CallError> {
    let cluster = Cluster::new(nodes, tip, Policy::default())?;
    let send = Request::once("sendrawtransaction", json!([signed]));
    cluster.broadcast(send).await
}
```

`Cluster::primary` is the primary's index in the order given, never an
endpoint. `Debug` shows the node count and that index only.

The `testing` feature adds `rpc::testing::Script`, a node double that
replies in order and asserts each method and its parameters. Enable it in
`[dev-dependencies]` only.

## Swap

`Swap` is a `Client` whose nodes are replaced while every holder keeps the
same handle: compose a chain over it once, then `Swap::replace` installs
another transport together with its nodes' `Statistics`. Each call finishes on
the transport it started on; later calls reach the replacement. Batches and
broadcasts are forwarded, so a swapped `Cluster` still broadcasts.

```rust
use std::sync::Arc;

use rpc::{Client, Endpoint, Swap};

fn repoint() -> Result<(), rpc::Error> {
    let first = Endpoint::new("http://127.0.0.1:8545").connect()?;
    let counters = vec![first.statistics()];
    let swap = Arc::new(Swap::new(Arc::new(first), counters));
    let held: Arc<dyn Client> = swap.clone(); // what the chain keeps

    let second = Endpoint::new("http://127.0.0.1:8546").connect()?;
    let counters = vec![second.statistics()];
    swap.replace(Arc::new(second), counters);
    drop(held);
    Ok(())
}
# repoint().expect("valid endpoints");
```

## Notes

- Capture `Http::statistics()` before wrapping it in retry/failover. Its
  cloneable handle samples attempts, failures, consecutive failures and a
  sanitized last failure from memory without locks or RPC. Every endpoint
  execution counts once, including batches; any transport or JSON-RPC error
  makes it a failure. Successful completions reset the consecutive counter.
  Invalid local parameters do not count; cancellation records no invented outcome.
  Label handles by configured ordinal, never by a credential-bearing URL.

- Submit a transaction with `Request::once` or `Batch::once`. `Retry` and
  `Failover` then execute it exactly once against one endpoint, because a
  repeated submission can send the same payment twice. Pass it to
  `Client::broadcast` to reach every node of a `Cluster` once; every
  decorator forwards a broadcast, so wrapping never narrows it to one node.
  When no node succeeds, a transport failure is returned before an error
  object, because that node may have accepted the envelope.
- An endpoint is its URL, like a database URI: no separate headers or
  references. It reaches the node as written, path and query included, so
  provider keys such as `/v3/<key>` or `?apikey=<key>` just work; `user:password`
  userinfo is sent as HTTP Basic authorization, and a `header.<Name>=value`
  query parameter is sent as that header instead. `Endpoint::parts` parses it.

## Injected events

Attach a scoped `logging::Logger` with `Http::with_logger`. The endpoint reports
sanitized failures, repeated-failure counts (at most every 30 seconds), and recovery.
Retries and failover preserve that node's context without duplicating its events.
No URL, request parameters or provider error prose reaches the sink.
