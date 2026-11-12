# 078: `packages/rpc` as small decorators

Status: steps 1–3 and 5–9 of section 8 are built; step 4 is dropped (see
"Built, and where it deviates"); steps 10–11 (payments composition API,
server routes) are open. Paths are under `packages/rpc/` unless noted. Line
numbers are as of `7e1e9a9`, plus the uncommitted `rpc.max_batch` diff,
which this design treats as current.

## Built, and where it deviates

- **`rpc::node(&Endpoint, Logger) -> Result<Node, Error>`**, in its own
  `src/node.rs`, replaces `Endpoint::connect`. `Endpoint` stays a value (the
  URL, `validate`, `bounds`, `parts`) and builds no client. A free function,
  not `Node::new`: `Node` is an alias of `Counted<…>`, whose `new` already
  wraps any client.
- **`base::Height` is unchanged** (`fn(&rpc::Http)`), so `sdk/base` and the
  chains' probes are untouched. `Split` and `Logged` expose `inner()`, as
  `Counted` does, and `sdk/payments` probes through the node type:
  `Cluster::new(nodes, move |node| height(node.inner().inner()), …)`.
- **`cause()` is a crate trait**, `statistics::Outcome::cause`, implemented
  for a call's, a batch's and a broadcast's outcome, so `Counted` and
  `Logged` need no widen/narrow.
- **`Counted` counts an attempt when it finishes**, not with a `begin()`.
  An outcome of `ErrorKind::InvalidRequest` (refused before sending: a bad
  parameter, an oversized body) is neither counted nor logged, which keeps
  "an invalid param is not counted" now that the counter sits above `Http`.
- **`Bounds` has no `validate`**; `Endpoint::validate`/`bounds` check the
  whole URL. `Parameter::setting` is added; a setting keeps its `rpc.` name.
- `Request::method`, `Batch::method`/`chunks`/`requests` are `pub(crate)`.
- `sdk/payments` builds each node with no log sink (`Logger::default()`),
  as before: `Builder::with_logger` is step 10.
- The server's API takes node URLs only (`RpcEndpoint`/`DetailedEndpoint`
  removed); `RpcNode` reads `endpoint.bounds()`. No `routes` column yet.

## The cluster moved to `sdk/base`

`rpc` is generic JSON-RPC, and a node of it need not have a tip. The old
`rpc::Cluster` ranked nodes by an injected tip probe and demoted by reading
`Counted`'s failure streak, so probes had to dig under the counters
(`node.inner()`). Now:

```
node     = rpc::node(endpoint, logger)        // Counted<Logged<Split<Http>>>
tipper   = config.node(Arc::new(node))        // the chain's Client: rpc::Client + base::Tip
cluster  = base::Cluster::new(tippers, Policy::default())   // rpc::Client + Tip
specific = Retry → Swap → the chain's full client over it
```

- `base::Tip: rpc::Client`: `tip()` (Bitcoin `getblockcount`, Ethereum
  `eth_blockNumber`, Solana `getSlot` at confirmed) and `info()`. Each
  chain's `Client` implements both; `Config::node` wraps a node in it.
  `Config::info` is gone: `Payments::info` is `config.node(..).info()`.
- `base::Cluster` keeps the old behaviour (lazy first probe, highest tip,
  tie keeps the incumbent, `max_lag`, refresh, failure threshold, rate-limit
  demotion, one probe round at a time) and its own failure streak for the
  primary, from the results it forwards. `Policy` moved with it and holds
  `probe_timeout` (was `rpc::PROBE_TIMEOUT`).
- Probes call `node.tip()` through the full stack, so they show in
  `Counted` statistics. `Counted`/`Logged`/`Split::inner()`, `Cluster::one`
  and `chain_ethereum::height` are deleted.

`Http` does five jobs today: transport, batch splitting, statistics, failure
logging and rate-limit mapping (`src/http.rs:26-177`). `Endpoint` mixes
settings written in the URL with a separate `bounds` object
(`src/endpoint.rs:52-127`). `Cluster` keeps a second set of counters for each
node (`src/cluster.rs:83-86`). This design gives every job one decorator,
puts every node setting in its URL, and adds a `Router`. Routing is data the
server stores and composes. It is never part of `rpc`, and never part of a URL.

## Decisions first

1. **One `Client` decorator per responsibility.** `Http` is only the
   transport. `Split`, `Counted`, `Logged`, `Retry`, `Cluster`, `Router` and
   `Swap` each do one thing and are each a `Client`.
2. **The URL carries the node, and only the node**: userinfo, `header.*`,
   timeout, body sizes and `max_batch`. `rpc::node(&endpoint, logger)` turns
   one URL into that node's preset, `Counted<Logged<Split<Http>>>` (the
   `rpc::Node` type).
3. **`Bounds` moves into the URL** as `rpc.*` parameters. The `{url, bounds}`
   object form goes away, so a node is one string.
4. **`Failover` is deleted.** Nothing in production uses it. `Router` takes
   over its walk ("advance on a retryable failure").
5. **`Router`**: a default client, plus routes of `{methods → clients}`. Each
   route round-robins over its clients. A retryable failure moves to the next
   client in the rotation, then to the default.
6. **The composition lives outside `rpc`.** `sdk/payments` owns the default
   per-network stack, `Retry<Cluster<Node>>`. `apps/server` stores routes next
   to its nodes and wraps the default in a `Router` when routes exist.

## 1. Inventory today

| Responsibility | Where |
|---|---|
| `Client` trait, `Arc<C>` impl, `narrow`/`widen` | `src/client.rs:17-67` |
| `Request`, `Batch`, `Repetition`, timeouts (`BATCH_*`, `PROBE_TIMEOUT`, `SEND_TIMEOUT`) | `src/client.rs:69-220` |
| `Call` | `src/lib.rs:32-50` |
| jsonrpsee client build: headers, timeout, size limits | `src/http.rs:49-70` |
| Per-call timeout (`within`) | `src/http.rs:192-202` |
| Params validation and serialization | `src/http.rs:204-231`, `139-148` |
| jsonrpsee error → `ErrorKind` (`Error::from_rpc`) | `src/http.rs:255-289` |
| Error object → `Failure`, unanswered batch call | `src/http.rs:235-253` |
| Rate-limited answer → `ErrorKind::RateLimited` (single, batch) | `src/http.rs:99-103`, `166-175`; `src/error.rs:60-68` |
| **Batch split by `max_batch`; `max_batch=1` sends calls alone** | `src/http.rs:106-137` |
| **Statistics recorded inside the transport** | `src/http.rs:89,98,149,165`; `src/statistics.rs` |
| **Failure-streak logging inside the transport** | `src/http.rs:30,80-83,98,165`; `src/events.rs` |
| URL parse: `header.*`, `rpc.max_batch`, validation, `Parts` for the UI | `src/endpoint.rs:12-50`, `162-292` |
| `Bounds` (timeout, request/response bytes) as a separate serde object | `src/endpoint.rs:52-101`, `294-341` |
| `Retry`, `Backoff`, `Trigger` | `src/retry.rs` |
| `Failover` (ordered walk) | `src/failover.rs` |
| `Cluster`: primary by height, demotion, refresh, broadcast fan-out, **own per-node `Statistics`** | `src/cluster.rs` |
| Rate-limit codes for demotion (`Limit`, `Statistics::limited`) | `src/cluster.rs:21-28,40`; `src/statistics.rs:83-94` |
| `Swap`: hot replacement, carries the nodes' `Statistics` | `src/swap.rs` |
| `Error`, `ErrorKind`, `CallError`, `is_retryable` | `src/error.rs` |
| `RawJson`, `Failure`, `is_rate_limited`, `is_server_error` | `src/value.rs` |
| `Script`, `Faults` doubles | `src/testing/` |

Findings while reading:

- **No production code ever calls `Http::with_logger`.** RPC failures are
  logged nowhere.
- **Statistics are counted twice**: once per HTTP request (`Http`) and once
  per routed call (`Cluster` `Node`). `Swap::statistics` is never read, and
  `/v1/networks` shows no RPC statistics today.
- **`Failover`** appears only in the README and in a stale line of
  `apps/server/README.md:30-32`.
- `Trigger::Server`/`Remote` and `Retry::with_attempts` have no caller
  outside `rpc`.
- `base::Height` is `fn(&rpc::Http)` (`sdk/base/src/config.rs:136`), so the
  probe is tied to the transport type.

## 2. The decorators

The stack, from the outside in:

```
Swap ─ Retry ─ [Router] ─ Cluster ─┬─ Node 0 = Counted ─ Logged ─ Split ─ Http
                         └ routes ─┴─ Node 1 = …        (clones share everything)
```

| Decorator | One responsibility | Wraps | Config from | File |
|---|---|---|---|---|
| `Http` | JSON-RPC over HTTP to one node, with error mapping | – | URL | `http.rs` |
| `Split<C>` | Sends a batch in requests of at most N calls | one node | URL `rpc.max_batch` | `split.rs` |
| `Counted<C>` | Counts outcomes into a `Statistics` handle | one node | – | `statistics.rs` |
| `Logged<C>` | Logs failure streaks and recovery | one node | injected `Logger` | `events.rs` |
| `Retry<C>` | Repeats retryable work with backoff | any | code (payments preset) | `retry.rs` |
| `Cluster<C>` | Primary by height; broadcast to every node | `Vec<Counted<C>>` | code (`Policy`) | `cluster.rs` |
| `Router` | Sends listed methods to their own rotation | default + clients | server DB | `router.rs` |
| `Swap` | Replaces the stack live | `Arc<dyn Client>` | – | `swap.rs` |

Every decorator forwards `broadcast` explicitly. The rule is unchanged
(`src/client.rs:24-33`).

### `Http`: transport only

```rust
#[derive(Clone)]
pub struct Http { client: HttpClient, url: String }

impl Http {
    pub(crate) fn new(target: &str, url: String, headers: HeaderMap, bounds: Bounds) -> Result<Self, Error>;
}
impl Client for Http {
    fn request(..)  // Params::new → within(timeout) → Ok/Failure/from_rpc → Error::limited
    fn batch(..)    // one JSON-RPC batch, as sent; any rate-limited entry fails it
}
```

These stay inside `Http`, and why:

- **`Error::from_rpc` and `Failure::try_from`.** Only the transport sees the
  jsonrpsee error, so it must translate it.
- **Rate-limit mapping** (`Error::limited`, single and batch). Every layer
  above makes its decision on `Error::is_retryable()`: `Retry` backs off,
  `Router` advances, `Cluster` demotes, `Counted` records the code. If a
  decorator did the mapping instead, layers below it would classify the same
  answer differently, and a stack built without that decorator would retry
  nothing. The node's answer is translated once, at the boundary.
- **Per-call timeout and body limits.** Both are properties of the HTTP
  execution.
- **Params validation.** An invalid call fails before any byte is sent, so
  it is counted nowhere.

These leave `Http`: `max_batch` and the chunk loop (to `Split`), `statistics`
and `begin` (to `Counted`), `events` and `with_logger` (to `Logged`). `Http`
goes from about 290 lines to about 200.

### `Split`: batch size

```rust
#[derive(Clone)]
pub struct Split<C> { inner: C, max: Option<NonZeroUsize> }

impl<C: Client> Client for Split<C> {
    fn request(..)   // forwarded
    fn batch(&self, batch) {
        let Some(max) = self.max.filter(|max| max.get() < batch.len()) else { return inner.batch(batch) };
        if max.get() == 1 { /* each call: inner.request(call), in order */ }
        for chunk in batch.chunks(max) { results.extend(inner.batch(chunk).await?) }  // first failure fails all
    }
    fn broadcast(..) // forwarded
}
```

`client.rs` gains `Batch::chunks(self, NonZeroUsize) -> Vec<Batch>` and
`Batch::requests(self) -> Vec<Request>`. Both keep the batch's `Repetition`
and timeout, so every chunk runs under the batch's timeout, as today
(`src/http.rs:106-121`). A `Forbidden` batch is still sent to one node, once.

### `Counted`: statistics

```rust
#[derive(Clone)]
pub struct Counted<C> { inner: C, statistics: Statistics }

impl<C> Counted<C> {
    pub fn new(inner: C) -> Self;
    pub fn statistics(&self) -> Statistics;   // shared by every clone
    pub fn inner(&self) -> &C;                // what Cluster probes
}
impl<C: Client> Client for Counted<C> {
    // begin(); outcome = inner.*; record(&outcome); outcome
    // broadcast: widen → record → narrow
}
```

`statistics.rs` keeps `Statistics` and `Snapshot`, and adds `Counted` and one
pure function, `cause(&outcome) -> Option<Cause>`, where
`Cause { code: u64, text: String }`. It is the sanitized classification that
`Statistics::finish` and `Logged` both use, and it is the code that today
returns from `Statistics::call` and `Statistics::batch`. `Statistics::limited`
is deleted (see `Cluster`).

`Counted` sits above `Split`, so one counted execution is one logical call or
batch, not one HTTP request. A node shared by `Cluster` and a `Router` route
is one clone, so every path updates the same counters.

### `Logged`: events

```rust
#[derive(Clone)]
pub struct Logged<C> { inner: C, events: Arc<Events> }

impl<C> Logged<C> { pub fn new(inner: C, logger: Logger) -> Self; }
impl<C: Client> Client for Logged<C> {
    // outcome = inner.*; events.outcome(cause(&outcome).map(|c| c.text)); outcome
}
```

`Events` keeps its streak and its 30 s throttle (`src/events.rs:46-74`). The
logger arrives already scoped (`network`, `node` = index, never the URL).

### `Retry`: keep

`Retry` is unchanged. It sits outermost under `Swap`, so one attempt is one
full walk of the `Router` or `Cluster`. Each retry starts the rotation one
step further. Deleting the unused `Trigger` variants is an open question
(section 9).

### `Failover`: delete

`Cluster` replaced it for whole-network use (`sdk/payments/src/payments.rs:415-426`),
and `Router` now owns the walk. The README example moves to
`Retry<Cluster>`. About 120 lines are removed.

### `Cluster`: keep, simplify

```rust
pub struct Cluster<C> { first: Counted<C>, rest: Vec<Counted<C>>, selection: Option<Selection<C>> }

impl<C: Client> Cluster<C> {
    pub fn new<P>(nodes: Vec<Counted<C>>, probe: P, policy: Policy) -> Result<Self, Error>
    where P: for<'a> Fn(&'a C) -> BoxFuture<'a, Result<u64, CallError>> + Send + Sync + 'static;
    pub fn one(node: Counted<C>) -> Self;
    pub fn statistics(&self) -> Vec<Statistics>;   // the nodes' own counters
}
```

- The private `Node { client, statistics }` (`src/cluster.rs:83-86`) is
  deleted, along with the manual `begin`/`call`/`batch` bookkeeping in
  `unicast`, `unicast_batch` and `fan_out` (`167-216`). `observe` reads
  `node.statistics().snapshot().consecutive_failures`.
- Demotion on a rate limit reads the outcome itself:
  `Err(e) if e.is_rate_limited()`, where the new `Error::is_rate_limited` is
  `RateLimited(_) | HttpStatus(429)`. This deletes `Limit`,
  `Policy::rate_limit_codes` and `Statistics::limited`. One behavior widens:
  a rate-limited `-32003` now demotes too.
- The probe runs on `node.inner()`, beneath the counters, as today: probes
  never count towards demotion.

### `Swap`: keep

`Swap` stays as it is: `new` and `replace` take the stack plus its nodes'
`Statistics`.

### `Router`: new (section 4)

## 3. The essentials in the URL

| Written in the URL | Meaning | Default |
|---|---|---|
| scheme, host, port, path, query | sent as written (provider keys) | – |
| `user:password@` | HTTP Basic | – |
| `header.<Name>=v` | static header, removed from the URL | – |
| `rpc.timeout_ms=N` | one execution's limit | 15000 |
| `rpc.max_request_bytes=N` | request body limit | 10 MiB |
| `rpc.max_response_bytes=N` | response body limit | 64 MiB |
| `rpc.max_batch=N` | most calls per request; 1 sends no batches | none |

These never go in a URL: retry policy, cluster policy, routes and the logger.
They belong to the composer, not to the node.

```rust
/// One node: its URL. Serializes as the string.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Endpoint { pub url: String }

/// The typed view of a URL's `rpc.*` settings; no serde.
pub struct Bounds { pub timeout_ms: u64, pub max_request_bytes: u32,
                    pub max_response_bytes: u32, pub max_batch: Option<NonZeroUsize> }

impl Endpoint {
    pub fn new(url) -> Self;
    pub fn validate(&self) -> Result<(), Error>;          // URL, headers, every rpc.* setting
    pub fn bounds(&self) -> Result<Bounds, Error>;        // the UI shows timeout_ms etc.
    pub fn parts(&self) -> Option<Parts>;                 // unchanged, plus Parameter::setting
}

// src/node.rs
/// A node as `node` builds it.
pub type Node = Counted<Logged<Split<Http>>>;

/// The preset: Http → Split → Logged → Counted. Does no I/O.
pub fn node(endpoint: &Endpoint, logger: Logger) -> Result<Node, Error>;
```

**Why `Bounds` moves into the URL.** The user asked for it: "the URL has
headers, timeouts, some essentials". A node becomes one string, like a
database URI, in the API, the DB JSON and the config files. The change
removes the `Form` visitor (`src/endpoint.rs:294-341`), `Bounds`' serde,
`with_bounds`, and the server's `RpcEndpoint`/`DetailedEndpoint` enum
(`apps/server/src/api/network_endpoints.rs:31-63`), about 100 lines in all.
The parser that already handles `rpc.max_batch` (`228-249`) is generalized to
a table of four settings. No stored data needs a fold: a schema change means
a fresh file (`apps/server/init.sql:1-3`), and `seed.sql:44-49` stores plain
URLs.

`Parameter` gains `setting: bool`, so the UI no longer lists `rpc.max_batch`
as a query parameter sent to the node.

**The preset order** is `Counted<Logged<Split<Http>>>`:

- `Split` sits right above `Http` because it is part of how this node is
  spoken to.
- `Logged` and `Counted` sit above `Split`, so both see logical calls. Their
  relative order makes no semantic difference.
- `Counted` is outermost so that `Cluster` can name its nodes
  `Vec<Counted<C>>` and read their counters.

## 4. `Router`

```rust
pub struct Router { default: Arc<dyn Client>, routes: Vec<Route> }

struct Route { methods: BTreeSet<String>, clients: Vec<Arc<dyn Client>>, next: AtomicUsize }

impl Router {
    pub fn new(default: impl Client + 'static) -> Self;
    /// `methods` go to `clients`, round robin.
    /// # Errors InvalidConfiguration: no methods, no clients, a method already routed.
    pub fn route(self, methods: impl IntoIterator<Item = impl Into<String>>,
                 clients: Vec<Arc<dyn Client>>) -> Result<Self, Error>;

    async fn send(&self, request: Request) -> Result<CallResult, Error> {
        let Some(route) = self.find(request.method()) else { return self.default.request(request).await };
        if request.repetition() == Repetition::Forbidden {
            return route.pick().request(request).await;
        }
        for client in route.rotation() {
            match client.request(request.clone()).await {
                Err(error) if error.is_retryable() => continue,
                outcome => return outcome,
            }
        }
        self.default.request(request).await
    }
}

impl Route {
    fn rotation(&self) -> impl Iterator<Item = &Arc<dyn Client>> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len();
        (0..self.clients.len()).map(move |step| &self.clients[(start + step) % self.clients.len()])
    }
}
```

`client.rs` gains `Request::method(&self) -> &str` and
`Batch::method(&self) -> Option<&str>`, which returns the one method shared
by every call, if there is one.

| Question | Answer |
|---|---|
| Unrouted method | Goes to the default only. |
| Routed method | Round-robin start, then the rest of the rotation, then the default. |
| Batch, every call the same method | Routed as one unit to one client: one rotation step. `Split` beneath still applies. |
| Mixed batch | Goes to the default, whole. |
| Rate-limited or failing routed client | `is_retryable()` (timeout, unavailable, 429/502/503/504, `RateLimited`): skip to the next client in the rotation. After the last one, the default. An error object, or a terminal transport error, is the answer. |
| `Repetition::Forbidden` | The picked client only, once, with no fallback (same rule as `Retry` and the old `Failover`). |
| `broadcast` | Always the default, so a `Cluster` still reaches every node. A route never narrows a broadcast. |
| Does the default join a rotation? | **No.** Recommended: to include a node, list its index in the route. The default is the fallback only. This keeps "prefer the primary" for everything else, and the rotation stays explicit data. |
| Thread safety | Routes are immutable after `route`. One `AtomicUsize` per route (`Relaxed`; fairness, not ordering). `Router: Send + Sync`. A configuration change builds a new stack and calls `Swap::replace`. |
| Where routes come from | Code only. No `rpc.*` URL parameter routes anything. |

Example: "only `getProgramAccounts` is split across both nodes; everything
else prefers the primary":
`Router::new(cluster).route(["getProgramAccounts"], vec![node0, node1])`.

## 5. Composition outside `rpc`

### `sdk/payments`: the default stack

Today: `cluster()` connects, collects `Http::statistics`, and returns
`Retry<Cluster<Http>>` (`sdk/payments/src/payments.rs:415-426`). The new
version splits that into the steps the server needs:

```rust
impl Builder { pub fn with_logger(self, logger: Logger) -> Self; }

impl Payments {
    /// Each endpoint's preset node; logged under `network` and its index.
    pub fn nodes(&self, network: &Network, endpoints: &[rpc::Endpoint]) -> anyhow::Result<Vec<rpc::Node>>;
    /// The default over `nodes`: a Cluster ranked by the chain's height.
    pub fn cluster(&self, network: &Network, nodes: Vec<rpc::Node>) -> anyhow::Result<rpc::Cluster<rpc::Logged<rpc::Split<rpc::Http>>>>;
    /// Every later call of `network` reaches `Retry::new(client)`; counters are `nodes`'.
    pub fn install(&self, network: &Network, client: impl rpc::Client + 'static, nodes: &[rpc::Node]) -> anyhow::Result<()>;
    /// The preset: install(cluster(nodes(endpoints))). Unchanged signature.
    pub fn set_endpoints(&self, network: &Network, endpoints: &[rpc::Endpoint]) -> anyhow::Result<()>;
}
```

`Retry` stays the preset's outermost layer, applied by `install`.
`base::Height` stays `fn(&rpc::Http)`: the cluster's probe reaches the
transport through the node type, `node.inner().inner()` (built that way).

### `apps/server`: routes as data

Storage goes on the same row as the nodes, because the routes name positions
in that list and must change atomically with it:

```sql
CREATE TABLE network_endpoint (
    chain   TEXT NOT NULL,
    network TEXT NOT NULL,
    nodes   TEXT NOT NULL,                -- JSON: ["https://…?rpc.max_batch=10", …]
    routes  TEXT NOT NULL DEFAULT '[]',   -- JSON: [{"methods":["getProgramAccounts"],"nodes":[0,1]}]
    …
```

An empty `routes` list is the default preset, with no `Router` layer. `seed.sql`
seeds `[]`.

Model (`src/models/endpoint.rs`), which owns its queries and its composition:

```rust
pub(crate) struct RpcRoute { methods: Vec<String>, nodes: Vec<usize> }
pub(crate) struct RpcEndpoints { nodes: Vec<rpc::Endpoint>, routes: Vec<RpcRoute> }

impl RpcEndpoints {
    /// Checks nodes as today, and each route: methods and nodes non-empty,
    /// every index < nodes.len(), no method in two routes.
    pub(crate) fn new(nodes: Vec<rpc::Endpoint>, routes: Vec<RpcRoute>) -> Result<Self, String>;

    /// Builds and installs the network's stack; asks no node.
    pub(crate) fn install(&self, payments: &Payments, network: &Network) -> anyhow::Result<()> {
        let nodes = payments.nodes(network, &self.nodes)?;
        let cluster = payments.cluster(network, nodes.clone())?;
        if self.routes.is_empty() {
            return payments.install(network, cluster, &nodes);
        }
        let mut router = rpc::Router::new(cluster);
        for route in &self.routes {
            let clients = route.nodes.iter().map(|&i| Arc::new(nodes[i].clone()) as Arc<dyn rpc::Client>).collect();
            router = router.route(route.methods.clone(), clients)?;
        }
        payments.install(network, router, &nodes)
    }
}
```

Callers: `api/network_endpoints.rs:373` `install()` and the new routes
handler call `RpcEndpoints::install`. After `Payments::build`, `boot`
(`src/main.rs:95`) calls it for every network that has routes. The `Connect`
seam that tests use (`src/main.rs:37-44`) is unchanged.

API and UI surface:

| Route | Change |
|---|---|
| `GET …/{chain}/{network}/endpoints` | Response gains `routes: [{methods, nodes}]`. `RpcNode` keeps `timeout_ms`, … (read from the URL) and gains `max_batch`. |
| `PUT …/{chain}/{network}/routes` | New. Body is `[{methods, nodes}]`. Validate, store, install. No node is asked. |
| `POST`/`PUT …/endpoints` | Body items become URL strings only (`DetailedEndpoint` goes away). A list that leaves a route pointing past its end is refused. |
| `DELETE …/endpoints/{index}` | Refused while a route names `index`: "node 1 is routed (getProgramAccounts); change routes first". |
| UI `pages/networks/_components/nodes.tsx` | A "Routes" list: methods, then node chips; one edit dialog. |

## 6. File layout after

```
packages/rpc/src/
  lib.rs         re-exports; Call
  client.rs      Client trait; Request, Batch (chunks, method), Repetition, timeouts
  value.rs       RawJson, Failure, CallResult
  error.rs       Error, ErrorKind (+ is_rate_limited), CallError
  endpoint.rs    the URL: headers, rpc.* settings (Bounds), Parts
  node.rs        Node and node(): the preset from one URL
  http.rs        Http: one node over HTTP, jsonrpsee errors and rate limits mapped
  split.rs       Split: a batch in requests of at most max_batch calls
  statistics.rs  Statistics, Snapshot, Counted, Outcome::cause
  events.rs      Logged: failure streaks and recovery to the Logger
  retry.rs       Retry, Backoff, Trigger
  cluster.rs     Cluster, Policy: primary by height, broadcast to every node
  router.rs      Router: methods to their own rotation, else the default
  swap.rs        Swap: replace the stack live
  testing/       Script (scripted node), Faults (failures on demand)
```

`failover.rs` is deleted. Net code: about −220 lines (Failover, Bounds serde,
Cluster bookkeeping, Limit) and about +170 lines (Split, Counted, Logged,
Router).

## 7. Testing

Every decorator is tested in its own file's `#[cfg(test)]` module, over
`testing::Script`. `Faults` wraps it where a test needs failures or request
counts. `tests/http.rs` keeps the real-socket transport tests, plus one preset
test per URL setting.

| Decorator | Tests |
|---|---|
| `Http` (`tests/http.rs`) | As today (`139-424`). The four `max_batch` tests (`460-525`) stay as **preset** tests through `rpc::node`. One more checks that `rpc.timeout_ms` is applied and never sent. |
| `Split` | Over `Faults<Script>`: 5 calls at max 2 give `switch.requests() == 3`, answers in order. Max 1 gives three `request`s. A batch within the limit is one request. A failing chunk fails the whole batch. A chunk keeps `Forbidden` and the timeout (`Script::once_only`). |
| `Counted` | Ok, error object, transport error and rate limit each give the expected `Snapshot`. A broadcast is counted. Clones share counters. An invalid param is not counted. |
| `Logged` | `logging::memory` sink: first failure warns, a repeat inside 30 s is silent, then a count; success gives `rpc.recovered`. No URL or params in any field. |
| `Retry` | `Faults::fail_next(Transport(Timeout))` gives 2 attempts. `Forbidden` runs once. A batch repeats on transport errors only. |
| `Cluster` | Nodes are `Counted::new(Faults::new(Script))`. Probe picks the highest tip; a tie keeps the incumbent; rate-limit demotion via `Fault::Transport(RateLimited)`; threshold; refresh; broadcast reaches every node once. These are today's chain wallet test cases, moved into `rpc`. |
| `Swap` | As today (`src/swap.rs:90-112`). |

`Router` tests:

1. An unrouted method reaches the default only (`Script::exhausted` on every client).
2. A routed method alternates A, B, A, B.
3. A same-method batch reaches one client as one batch. A mixed batch reaches the default.
4. A retryable failure on A goes to B. Both failing goes to the default. An error object on A is the answer, and B is never asked.
5. `Forbidden` reaches the picked client only, and its failure is returned.
6. `broadcast` reaches the default only.
7. Configuration errors: a method in two routes, no clients, no methods.
8. 100 concurrent calls over 2 clients give 50 each (`Faults::switch().calls`).

## 8. Migration

Each step builds and passes `cargo test --workspace` on its own.

| # | Step | Public API change for callers |
|---|---|---|
| 1 | `Error::is_rate_limited`; `statistics::cause()`; `Events` reads it | none |
| 2 | Add `Counted` and `Logged` with tests. `Http` still counts and logs. | new types only |
| 3 | `Cluster::new`/`one` take `Counted<C>`; drop `Node`; probe gets `&C` | chain wallet tests and `apps/server/tests/support/mod.rs:111` wrap nodes in `Counted::new`; `payments::cluster` too |
| 4 | ~~`base::Height` → `fn(&dyn Client)`~~ dropped: the probe goes through `node.inner().inner()` | none |
| 5 | Add `Split` and `Batch::chunks`/`requests`/`method`, `Request::method`. `Http` loses `max_batch`, statistics and events. `rpc::node(&endpoint, logger) -> Node`. | `Http::statistics`/`with_logger` and `Endpoint::connect` removed → `rpc::node(…)`, `Node::statistics()` (`payments.rs:107,421`) |
| 6 | `Bounds` into the URL (`rpc.timeout_ms`, …); `Endpoint` serializes as a string; `Parameter::setting` | `Endpoint::with_bounds` and the `bounds` field are removed; server drops `DetailedEndpoint`; `RpcNode::new` reads `endpoint.bounds()` |
| 7 | Rate-limit demotion by `ErrorKind`; delete `Limit`, `Policy::rate_limit_codes` | `rpc::Limit` removed |
| 8 | Delete `Failover`; README rewritten around the preset; fix `apps/server/README.md:30-32` | `rpc::Failover` removed |
| 9 | Add `Router` with tests | new type |
| 10 | payments: `Builder::with_logger`, `nodes`, `cluster`, `install` | additive; `set_endpoints` keeps its signature |
| 11 | server: `routes` column, `RpcRoute`, `RpcEndpoints::install`, `PUT …/routes`, removal guard, UI | API additions; `endpoints` body is URL strings only |

## 9. Open questions

| Question | Recommendation |
|---|---|
| Should `Cluster` fail over within one call? Today a dead primary fails every `Retry` attempt until `failure_threshold` demotes it. | Not in this refactor. If wanted later, `Cluster` should walk the eligible nodes on a retryable error, as `Router` does. |
| Delete `Trigger::Server`/`Remote` and `with_attempts`? They have no caller outside `rpc`. | Yes, as a later simplify step: `Retry` repeats `is_retryable()` failures only. |
| Should probes count in node statistics? | No. They run on `Counted::inner`, as today, so probes never demote. |
| Removing a node that a route names | Refuse, and say which route names it. Do not remap indexes silently. |
| Should chains ship default routes (such as Solana `getProgramAccounts`)? | No. The default preset is unrouted; routes are server data only. |
| Exact method names, or prefixes and wildcards? | Exact names only. A prefix can come later if a provider needs one. |
| Show node statistics on `/v1/networks`? Today nothing reads them. | Yes, cheaply: each `RpcNode` gains its `Snapshot` from `Swap::statistics()` by index. A separate change. |
| The fallback default may re-hit a route client that just failed (the primary is one of them). | Accept it: one extra attempt, and `Cluster` demotes it under the usual rule. |
| How does the logger reach the preset? | `Builder::with_logger`. `boot` passes its logger, scoped by network and node index. |
