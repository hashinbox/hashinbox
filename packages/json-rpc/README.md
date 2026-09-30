# json-rpc

A JSON-RPC client over HTTP with timeouts, size limits, retry and failover.
Chain code is written against the `Client` trait, so a test can substitute a
scripted node.

## Usage

```rust
use std::collections::BTreeMap;

use json_rpc::{CallError, Client, Endpoint, Failover, Request, Retry};
use serde_json::json;

async fn block_count(rpc: &dyn Client) -> Result<u64, CallError> {
    let raw = rpc.request(Request::new("getblockcount", json!([]))).await??;
    Ok(raw.deserialize()?)
}

fn connect() -> Result<impl Client, json_rpc::Error> {
    // The application supplies secret values; this crate reads no environment.
    let secrets = BTreeMap::from([("RPC_KEY".to_owned(), "secret".to_owned())]);

    let primary = Endpoint::new("https://node.invalid/v3/${RPC_KEY}").connect(&secrets)?;
    let backup = Endpoint::new("https://backup.invalid/v1")
        .with_header("X-API-Key", "${RPC_KEY}")
        .connect(&secrets)?;

    // Three attempts; one attempt tries each endpoint in order.
    Ok(Retry::new(Failover::new(vec![primary, backup])?))
}
# connect().expect("endpoints are valid");
```

The `testing` feature adds `json_rpc::testing::Script`, a node double that
replies in order and asserts each method and its parameters. Enable it in
`[dev-dependencies]` only.

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
  repeated submission can send the same payment twice.
- Write a secret as a `${NAME}` reference in the URL or a header value, never as
  literal text. `Endpoint::connect` resolves it, and rejects a value placed in
  the URL that contains a delimiter such as `/`, `@` or `?`, because it would
  change which host or path the request reaches.
- The URL reaches the node as written, path and query included. Provider keys
  such as `/v3/<key>` or `?apikey=<key>` work without any extra setting.
