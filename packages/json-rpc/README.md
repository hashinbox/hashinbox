# json-rpc: one URL per node, retry and failover as decorators

## Why this crate exists

Every chain talks to its node over JSON-RPC and needs the same endpoint
policy: timeouts, request and response size bounds, custom headers, bounded
retries of transport failures, and ordered failover across nodes, plus a way to
opt out of all of that for an operation that must not repeat (raw transaction
submission). This crate owns that policy and a two-method [`Client`] trait
chain code is written against, so tests can substitute a double.

One layer does one thing. [`Http`] executes one call against one node;
[`Retry`] repeats; [`Failover`] moves on. Each is a [`Client`], so the
composition root writes `Retry::new(Failover::new(vec![a, b])?)` and every other
layer keeps the same contract.

It does not own JSON-RPC framing, id correlation, response validation, or
batch ordering (`jsonrpsee` does), and it knows no chain methods: results stay
[`RawJson`] until a chain adapter decodes them. It also reads no environment:
the caller supplies values through [`Secrets`].

## Concepts

| Concept | Type | Notes |
|---|---|---|
| transport | [`Client`] | implemented by [`Http`], by the decorators, and by test doubles |
| what one call may take | [`Bounds`] | timeout and the two size limits |
| one node | [`Endpoint`] → [`Http`] | the provider's URL, the headers every call carries, and its [`Bounds`] |
| repetition | [`Retry`] with [`Backoff`] and [`Trigger`] | attempts of the inner client, with a delay between them |
| alternatives | [`Failover`] | endpoints in order; advances only on a retryable transport failure |
| what may repeat | [`Request`] / [`Batch`] carrying [`Repetition`] | the caller decides, and no layer may override it |
| one call in a batch | [`Call`] | method + array or object params |
| result | [`CallResult`] = `Result<`[`RawJson`]`, `[`Failure`]`>` | a server error object is a result, not a transport error |
| transport failure | [`Error`] / [`ErrorKind`] | [`Error::is_retryable`] |
| either failure | [`CallError`] | flattens both with `?` |
| a scripted node | `json_rpc::testing::Script` with `json_rpc::testing::Reply` | a [`Client`] a test substitutes; behind the `testing` feature |

`Retry::new(Failover::new(endpoints)?)` makes three attempts with 250 ms initial
backoff capped at 2 s, and one attempt is one complete round over the endpoint
list. [`Request::once`] and [`Batch::once`] carry [`Repetition::Forbidden`]:
exactly one execution against exactly one endpoint, whatever the decorators say.

## One node

```rust,ignore
Endpoint { url: String, headers: BTreeMap<String, String>, bounds: Bounds }
```

| Field | Type | Default |
|---|---|---|
| `url` | the provider's URL, `http` or `https`, naming a host and carrying no fragment | — |
| `headers` | header name → value, sent on every call | none |
| `bounds.timeout_ms` | `u64 > 0` | `15000` |
| `bounds.max_request_bytes` | `u32 > 0` | `10485760` |
| `bounds.max_response_bytes` | `u32 > 0` | `67108864` |

**The URL is the provider's and reaches the node unaltered.** Its path and its
query are the provider's own — `https://host/v3/<key>`, `?apikey=<key>`,
`?auth=<token>` — so this crate claims no parameter inside it and drops
nothing from it. What the deployment chooses is a field beside the URL, never a
word smuggled into it; the retry policy and the endpoint *list* are properties
of the deployment rather than of a node, so they stay outside [`Endpoint`]
altogether.

A deployment document writes a node either as the URL alone or as the object:

```json
["https://node.invalid/v3/${RPC_KEY}",
 {"url": "https://node.invalid/v1",
  "headers": {"X-API-Key": "${RPC_KEY}"},
  "bounds": {"timeout_ms": 20000}}]
```

A value carries a `${NAME}` reference (`[A-Z_][A-Z0-9_]*`), never a literal
secret; [`Endpoint::connect`] looks it up through [`Secrets`], which only the
composition root implements over the environment. A reference stands anywhere
in the URL — path, query, host or userinfo — and in any header value. One
resolved **into the URL** may hold only unreserved characters and `+=`: a value
carrying a delimiter is rejected, because it would either reach the node
percent-encoded or silently repoint the URL. Userinfo becomes HTTP Basic; a
credential that needs more than that belongs in a header.

## Public API map

| Group | Items |
|---|---|
| describe a node | [`Endpoint::new`], [`Endpoint::with_header`], [`Endpoint::with_bounds`], [`Endpoint::validate`], [`Endpoint::connect`] (the one way to build an [`Http`]), [`Bounds`] |
| compose | [`Retry::new`] (then [`Retry::with_attempts`], [`Retry::with_backoff`], [`Retry::with_trigger`]), [`Failover::new`] |
| call | [`Request::new`], [`Request::once`], [`Batch::new`], [`Batch::once`], [`Call::new`], [`Client::request`], [`Client::batch`], [`RawJson::deserialize`] |
| implement | [`Client`] (with [`BoxFuture`]) for a test double, [`Secrets`] for a value source |
| errors | [`Error`], [`ErrorKind`], [`Failure`], [`CallError`] |
| script a node (feature `testing`) | `Script::new`, `Script::once_only`, `Script::push`, `Script::requests`, `Script::methods`, `Script::exhausted`; `Reply::{ok, raw, failure, result}` and `Reply::expecting` |

## Features

| Feature | Default | What it adds |
|---|---|---|
| `testing` | off | `json_rpc::testing`: a scripted node double (`Script`, `Reply`) that replies in order and asserts the method, the exact parameters, the [`Repetition`] of a pinned method, and that the script was drained. It belongs in a `[dev-dependencies]` entry only — `cargo tree -p payment-server -e normal -f "{p} {f}"` shows `json-rpc default`, never `default,testing` |

A chain adapter keeps its own *fixtures* — the methods, parameters and
payloads its node speaks, and the network its client claims. What it does not
keep is the mechanism: order, parameter assertions, recording, exhaustion, and
the submission method pinned to [`Repetition::Forbidden`].

## Usage

```rust,no_run
use json_rpc::{Bounds, CallError, Client, Endpoint, Failover, Request, Retry, Secrets};
use serde_json::json;

async fn block_count(rpc: &dyn Client) -> Result<u64, CallError> {
    let raw = rpc.request(Request::new("getblockcount", json!([]))).await??;
    Ok(raw.deserialize()?)
}

# fn run(secrets: &dyn Secrets) -> Result<(), Box<dyn std::error::Error>> {
let nodes = [
    Endpoint::new("http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443")
        .with_bounds(Bounds { timeout_ms: 20_000, ..Bounds::default() }),
    Endpoint::new("https://backup.invalid/v1").with_header("X-API-Key", "${BTC_RPC_KEY}"),
];
let mut clients = Vec::new();
for node in &nodes {
    clients.push(node.connect(secrets)?);
}
let rpc = Retry::new(Failover::new(clients)?);
# let _ = rpc;
# Ok(())
# }
```

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | none | depends only on `jsonrpsee`, `serde`, `serde_json`, `tokio`, `url` |
| chains | `chain-bitcoin`, `chain-ethereum`, `chain-solana` | each `RpcClient::connect` wraps a [`Client`] |
| server, examples | `payment-server`, `payment-examples` | describe an [`Endpoint`] and compose the decorators |

Design notes: [`ARCHITECTURE.md`](../../ARCHITECTURE.md) (RPC transport configuration),
[`docs/CHAIN_LAYOUT.md`](../../docs/CHAIN_LAYOUT.md),
[`a retired design note (023).md`](../../a retired design note (023).md).
