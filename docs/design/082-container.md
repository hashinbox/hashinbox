# 082: Container and transport

Status: implemented. Builds on 078 (rpc decorators). Where the code differs from the first draft, this text follows the code; §10 lists those changes.

## 1. Goal

- **The container keeps every network in one place:** its service (indexer, wallet factory, broadcaster) and its transport. The server works only with the container, never with individual chains.
- **A chain reads a stored config once into its factory, then turns `(storage, client)` into its `Service`.** That's all it does.
- **A transport is a network's nodes, and `client()` selects among them.** It can be rebuilt at any time from what the database stores, with its state kept or refreshed.

## 2. Rules

1. `Transport::client()` returns a single `rpc::Client`. Every service call goes through it.
2. Selection is the transport's job. No rpc router; `rpc::Swap` stays in rpc but isn't used.
3. A transport can be built from the rows the database stores. Replacing endpoints rebuilds the selection as well.
4. **rpc decorators are stateless.** State (tips, failures, statistics, rotation) lives outside them, in the transport.
5. Whatever is fetched from the database can always rebuild the whole transport. Outside state, such as each node's tip, can be restored or refreshed.
6. Replacing endpoints is the same operation as building: rows in, nodes out, state kept for nodes that stay.
7. Each node carries its own state: its tip, its failure streak, its `Counted` statistics, and room for more.

## 3. Pieces

### rpc

```rust
impl Endpoint {
    /// A node's identity: `host[:port]/path`, without scheme, userinfo,
    /// `header.*` or `rpc.*` params. Providers put the network or key in the
    /// path, so different paths are different nodes.
    pub fn id(&self) -> String;
}

pub trait Client: Send + Sync {
    // request, batch, broadcast as today, plus:
    /// The one node behind this client; `None` over several.
    fn endpoint(&self) -> Option<&Endpoint> { None }
}
// Http returns its endpoint; Split, Logged, Counted and Retry forward their inner's.

impl Request { pub fn method(&self) -> &str; }          // public now: selection routes by it
impl Batch { pub fn method(&self) -> Option<&str>; }    // the one method every call names
```

`Router` is deleted. `Swap` stays in rpc as a small decorator, but nothing here uses it.

### base (`base::container`)

```rust
/// How a network gets its one client.
pub trait Transport: Send + Sync {
    fn client(&self) -> Arc<dyn rpc::Client>;
}

/// A client made upfront: tests, custom stacks. No nodes, no state.
pub struct Simple(pub Arc<dyn rpc::Client>);
impl Transport for Simple { fn client(&self) -> Arc<dyn rpc::Client> { self.0.clone() } }

/// What a network is (chain, network, chain_id, block_time, assets,
/// confirmations, settings). No endpoints: those belong to the transport.
pub struct NetworkConfig { … }

/// One chain: a stored config read once, then built into its Service.
pub trait ServiceFactory: Sized {
    const CHAIN: Chain;
    /// The chain's settings from a stored config, checked once.
    fn new(config: &NetworkConfig) -> anyhow::Result<Self>;
    /// Its service over `client`. Asks no node.
    fn service(&self, storage: &Storage, client: Arc<dyn rpc::Client>) -> anyhow::Result<Service>;
}

// Per chain: `chain_x::Factory { config: Config }`; `new` is `Config::try_from`,
// `Factory` builds everything (`service`, `chain`, `wallets`); `Factory::from(Config)` serves code-made configs.

/// How a chain reads one node.
pub trait InfoClient: rpc::Client {
    fn new(node: Arc<dyn rpc::Client>) -> Self where Self: Sized;
    fn tip(&self) -> Future<'_, anyhow::Result<u64>>;
    fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>>;   // reports, doesn't judge
}

/// An `InfoClient` whose tip is asked at most once per `ttl` (the network's
/// block time); between asks, the last answer. Uses `base::Cache`. Lives in
/// `info.rs`, exported as `base::CachedInfo`.
pub struct CachedInfo { inner: Arc<dyn InfoClient>, tip: Cache<u64> }
impl CachedInfo {
    pub fn new(inner: Arc<dyn InfoClient>, ttl: Duration) -> Self;
    pub async fn tip(&self) -> anyhow::Result<u64>;           // cached
    pub async fn last(&self) -> Option<u64>;                  // no call
    pub async fn info(&self) -> anyhow::Result<NodeInfo>;     // always asks
}

/// Every registered network: its service (or why it didn't build) and its transport.
pub struct Container<T: Transport> {
    storage: Storage,
    services: RwLock<Map<Network, Result<Arc<Service>, String>>>,
    transports: RwLock<Map<Network, Arc<T>>>,
}

impl<T: Transport> Container<T> {
    pub fn new(storage: Storage) -> Self;
    /// factory = F::new(&config), then service = factory.service(storage,
    /// transport.client()). A failure of either is kept with why. Asks no node.
    pub fn register<F: ServiceFactory>(&self, config: NetworkConfig, transport: T) -> &Self;
    pub fn service(&self, network: &Network) -> anyhow::Result<Arc<Service>>;
    pub fn transport(&self, network: &Network) -> anyhow::Result<Arc<T>>;
    // + networks, assets, stats, watch, generate, wallet(s), parse, broadcast
}
```

### apps/server (usecase)

```rust
/// A network's nodes, built from stored rows, and the selection over them.
#[derive(Clone)]
pub struct Transport(Arc<Nodes>);

impl Transport {
    /// No nodes yet. `I` is used only to make each node's reader. `config`
    /// gives the chain ID every node must serve and the block time the tips
    /// are cached for.
    pub fn new<I: InfoClient + 'static>(config: &NetworkConfig) -> Self;
    /// The stored nodes and routes: a node whose URL stays keeps its state; new
    /// ones start empty; missing ones go. The same call at boot and on every edit.
    /// A stored URL that does not build is left out and named in the error.
    pub fn load(&self, endpoints: &[Endpoint], routes: &[Route]) -> anyhow::Result<()>;
    /// Replaces one node with `endpoint`; it starts with empty state.
    pub fn replace(&self, node: &Node, endpoint: &rpc::Endpoint) -> anyhow::Result<()>;
    /// Asks the node at `endpoint` what it serves; fails unless its chain ID
    /// is this network's. Run before storing a new node.
    pub async fn verify(&self, endpoint: &rpc::Endpoint) -> anyhow::Result<NodeInfo>;
    /// The current nodes, in stored order.
    pub fn clients(&self) -> Clients;
    /// Ready clients in place of endpoints (tests); each still read by `I`.
    pub fn from_clients<I: InfoClient + 'static>(config: &NetworkConfig, clients: Vec<Arc<dyn rpc::Client>>) -> Self;
}

impl base::Transport for Transport {
    /// Retry over the selection: the same client for the transport's whole
    /// life, choosing a node on every call (below).
    fn client(&self) -> Arc<dyn rpc::Client>;
}

/// One node: its client, its cached reader, its state.
pub struct Node {
    client: Arc<dyn rpc::Client>,               // rpc::node(url): Http, Split, Logged, Counted
    info: CachedInfo,                           // CachedInfo::new(I::new(client), block_time)
    statistics: rpc::Statistics,                // the client's Counted
    health: Mutex<Health>,                      // retryable failures in a row, demoted since
    recovery: Duration,                         // max(10 × block_time, 30 s)
}
impl Node {
    pub fn endpoint(&self) -> Option<&rpc::Endpoint>;   // client.endpoint(); None for a ready client
    pub fn id(&self) -> Option<String>;                 // endpoint().id()
    pub async fn state(&self) -> NodeState;     // no call: last tip, failures, statistics
    pub async fn info(&self) -> anyhow::Result<NodeInfo>;
}

/// What we know about one node, for the API: a model (`models::NodeState`),
/// on each stored `models::Endpoint` as `state`.
pub struct NodeState {
    pub tip: Option<u64>, pub failures: u32,
    pub calls: u64, pub failed_calls: u64, pub last_failure: Option<String>,  // rpc::Snapshot
}

/// The current nodes: a list, plus the questions selection asks of it.
pub struct Clients(Vec<Arc<Node>>);             // Deref<Target = [Arc<Node>]>
impl Clients {
    /// The healthy node with the highest tip; ties and unknown tips: stored order.
    pub async fn highest(&self) -> Option<&Arc<Node>>;
    pub fn find(&self, id: &str) -> Option<&Arc<Node>>;
    /// Each stored endpoint with its node's state, for the API.
    pub async fn states(&self, endpoints: &mut [Endpoint]);
}
```

**Selection, on every request** (`client()`'s inner):

1. **A routed method** (e.g. `getBlock`): its route's nodes, rotated one per call, then the rest in stored order. No tip is asked.
2. **Anything else:** the healthy node with the highest tip first (`CachedInfo::tip`, so at most one tip call per node per block time, each at most 3 s; a failed tip counts as a failure), then the other healthy nodes in stored order, those within `confirmations` of that tip before the ones further behind or with no known tip, then the demoted ones.
3. **Demotion:** 3 retryable failures in a row, or one rate limit (`Error::is_rate_limited`), demote a node: it goes last. A success resets its streak; after `max(10 × block_time, 30 s)` it is back in order. A demoted node's tip is not asked.
4. **On failure:** a retryable failure moves to the next candidate; an error object is the answer; once-only work reaches the first candidate only.
5. **Broadcast** goes to every node, as today.

Inside, `Nodes` is `RwLock<Table { nodes: Vec<Arc<Node>>, routes }>`, the routes with their rotation counters, plus the chain ID (the network's name when it has none), the block time, `confirmations`, and the reader function `new::<I>` sets.

### apps/server (models)

`endpoint::db::find_all` already returns the network→endpoints map (`NetworkEndpoints`), and `endpoint_route::db::find_all` the network→routes map (`NetworkRoutes`).

### apps/server (usecase): `Networks`

```rust
/// Every stored network: its config, nodes and routes, as the database holds them.
pub struct Networks(Vec<models::Network>);      // each row carries its endpoints and routes

impl Networks {
    /// `network::db::find_all`: one read.
    pub async fn read(db: &Db) -> anyhow::Result<Self>;

    /// Every stored network of `F::CHAIN`: a transport read by `I`
    /// (`Transport::new::<I>`), loaded with its nodes and routes, registered
    /// with `F`. Main only, once per chain.
    pub fn register<I: InfoClient + 'static, F: ServiceFactory>(&self, container: &Container<Transport>) -> &Self;

    /// `network`'s stored nodes and routes into its existing transport
    /// (`Transport::load`). No chain types: the transport kept its reader.
    pub fn load(&self, network: &Network, container: &Container<Transport>) -> anyhow::Result<()>;
}
```

- The chain name comes from `F::CHAIN`, so there's no string argument and no way to pair the wrong name with a chain.
- `register` appears only in main. Edits call `load`, which needs no chain types.

## 4. Flows

**Boot (main):**

```rust
let container = Container::<usecase::Transport>::new(storage);
usecase::Networks::read(&db)
    .await?
    .register::<chain_bitcoin::Info, chain_bitcoin::Factory>(&container)
    .register::<chain_ethereum::Info, chain_ethereum::Factory>(&container)
    .register::<chain_solana::Info, chain_solana::Factory>(&container);
```

There's no bootstrap step. Tips fill in on first use, through the cache.

**`main.rs` is one `main` block,** with no `boot` function:
- read the configs, endpoints and routes;
- build the container with the lines above;
- build `AppState`;
- start the workers and the HTTP server.

`boot` exists today only so the tests can reuse it (`tests/support/mod.rs:72`). Under this design, the tests need neither `boot` nor `Container::<Simple>`:
- **Scripted clients go into the real transport:** `usecase::Transport::from_clients::<I>(&config, clients)` takes ready `rpc::Client`s in place of endpoints (each counted).
- **The worker wiring moves into the workers module:** `workers::executor(&container, &db, &queue, &actions, tick, logger)` builds the `Executor` with index, expire, prune, broadcast and trigger. It's the one piece both `main` and the test support call. It takes `AppState`'s fields, not `AppState`: `usecase` may not name `api` (`scripts/server-layers.py`).
- **The test support** builds its own container and `AppState` in a few lines, then calls `workers::executor`.

**Adding a node:**

```rust
let transport = container.transport(&network)?;
transport.verify(&endpoint).await?;             // wrong chain: refused, nothing stored
db.write(/* store it */).await?;
usecase::Networks::read(&db).await?.load(&network, &container)?;
```

**An endpoint or route edit:**

```rust
db.write(/* store the change */).await?;
usecase::Networks::read(&state.db).await?.load(&network, &state.container)?;
```

**Looking at and swapping one node:**

```rust
let nodes = container.transport(&network)?.clients();
nodes[0].state().await.tip;                     // its last tip, no call
nodes.highest().await;                          // the one selection prefers
nodes[0].info().await?;                         // ask it now: chain ID and tip
transport.replace(&nodes[0], new_endpoint)?;    // one node; the others keep their state
```

**The API:** `GET /v1/networks` and the endpoints routes give each stored node its `state` (`Clients::states`: tip, failures, calls; null for one not running); the network tab renders it next to its URL.

**Restart:** everything rebuilds from the two maps. Tips are refetched on first use. Failures and statistics restart at zero, as today. To keep tips across restarts, add a `tip` column to `network_endpoint` and seed the cache in `load`; that's left out for now.

**Tests:** server tests use `usecase::Transport::from_clients` with scripted clients, and `workers::executor`, as above. `Container::<Simple>` is for base and chain tests that need no server.

## 5. What moves, goes, or is renamed

| Today | After |
|---|---|
| `sdk/payments` (`Payments`, `Builder`) | `base::container` (`Container<T>`, `Transport`, `Simple`, `CachedInfo`) |
| `payments::Nodes`, `Payments::connect` | the server's `usecase::Transport` (`load`, `replace`) |
| `Payments::info` (the chain check when a node is added) | `usecase::Transport::verify` |
| `base::Cluster`, `Policy` (tip ranking, demotion) | deleted: the server's `Transport` selects; tips are cached in `CachedInfo`; failures in each `Node` |
| `rpc::Router` | deleted |
| `rpc::Swap` | kept in rpc as a decorator; no longer used by payments, the container or the server |
| `ServiceFactory::info(nodes)`, `ServiceFactory::build` (async) | `ServiceFactory::new(config)` and `service(&self, storage, client)` (sync), plus `CHAIN` |
| each chain's `impl ServiceFactory for Config` | `chain_x::Factory { config: Config }` builds (`service`, `chain`, `wallets`); `Config` is data only |
| each chain's wallet factory `chain_x::Factory` | renamed `chain_x::WalletFactory` (the name went to the service factory) |
| each chain's `Client` as an `InfoClient` | `Client` holds its network beside its `Info`; only `Info` is an `InfoClient` |
| `NetworkConfig.endpoints` | removed: endpoints belong to the transport |
| `InfoClient` (`tip`, `info`) | the same, plus `new(node)`; `info` no longer checks the chain ID (Bitcoin reports the network's name, `mainnet` for Core's `main`) |
| — | `rpc::Client::endpoint()`, `rpc::Endpoint::id()`; `Request::method`, `Batch::method` public |
| `main.rs` `boot` | gone: `main` is one block; `workers::executor` builds the jobs |

## 6. What base must allow

These need the user's approval:
- the `base::container` module;
- `Transport`, `Simple` and `CachedInfo`;
- the `ServiceFactory` change (sync, `CHAIN`, `new`, `service`);
- `InfoClient::new`;
- deleting `Cluster`/`Policy`;
- removing `NetworkConfig.endpoints`.

No new dependencies.

## 7. Migration

Each step builds and passes the tests.

1. **rpc:** `Endpoint::id`, `Client::endpoint`.
2. **base:** `InfoClient::new`, `CachedInfo`; split each chain's `Factory` from its `Config`; make `ServiceFactory` sync (`new`, `service`).
3. **base:** `container` with `Transport`, `Simple` and `Container<T>`; move `payments` into it.
4. **server:** `usecase::Transport` (`new`, `load`, `replace`, `verify`, `clients`) and its selection; `usecase::Networks` (`read`, `register`, `load`); main registers each chain.
5. **server:** the endpoint and route handlers use `verify` and `Networks::load`; the network tab shows each node's state.
6. Delete `Cluster`, `Policy`, `rpc::Router`, `Nodes`, `connect`, `NetworkConfig.endpoints` and `sdk/payments`.
7. Docs: the READMEs for rpc, base and the server.

## 8. Decided

| Question | Decision |
|---|---|
| Does `Endpoint::id` keep the path? | Yes. |
| A router in rpc? | No: `Router` is deleted; `Transport::client()` selects. |
| `Swap`? | Kept in rpc as a decorator (small, harmless); not used elsewhere. |
| Demotion after failures? | Yes: 3 consecutive retryable failures, or one rate limit, put a node last until a success. |
| How often are tips read? | On use, cached for the network's block time (`CachedInfo`). No update step. |
| Where is the chain ID checked? | `Transport::verify`, against the network's config, before a node is stored. |
| Node state in the API? | Yes: the network tab renders each node's tip, failures and statistics. |
| `ServiceFactory`: empty struct or instance? | An instance holding the chain's parsed `Config`: `new(&config)` once, then `service(&self, storage, client)`. Storage and client stay arguments (the container's, the transport's). |
| Does a demoted node come back only on success, or also after a timeout? | Also after `max(10 × block_time, 30 s)`, so a recovered node isn't stuck last forever when nothing else picks it. (A default taken during implementation, from the recommendation.) |
| Does a node lagging the highest tip by more than N blocks go after the others? | Yes, after every in-sync node, with N = the network's `confirmations`. (A default taken during implementation, from the recommendation.) |

## 9. Open questions

None.

## 10. Changes made while implementing

- `ServiceFactory::CHAIN` is a `base::Chain` (each chain's `CHAIN_NAME`), not a `&'static str`.
- `NodeState` is a model (`models::NodeState`), the API's wire shape: `statistics` is read into `calls`, `failed_calls` and `last_failure`. A node's streak and demotion time share one `Mutex<Health>`.
- `Node::endpoint()` and `id()` are `Option`s: a ready client (`from_clients`) has no URL.
- `Transport::load` keeps a node whose URL is unchanged. A URL edited in place (same id, new key or setting) is a new node, since its client must be rebuilt.
- Selection is the highest tip, then stored order (§3), so the lag rule decides failover order; ranking every node by tip alone would make it moot.
- With no `chain_id`, `verify` compares the node's reported chain to the network's name (Bitcoin).
- `Networks` holds the `models::Network` rows, which already carry their endpoints and routes.
- `workers::executor` takes `AppState`'s fields (layering).
- `CachedInfo` is in `base::info`, beside `InfoClient`.
- `Transport::replace` and `Clients::highest` have no caller outside tests yet.
- The server's scripted test chain gets its script through a thread-local, since `F::new` takes only the stored config. The suite maps each test chain to its reader and factory (`support::Served`).
- The handler switch to `verify` and `Networks::load` landed with step 4: the old path (`payments::Nodes`, `connect`) had no transport left to drive. Node state in the API came with it, and step 5 added the network tab, the OpenAPI and the UI types.
- Steps 2 and 3 ran on interim code removed in step 6: `payments` kept `Nodes` and a `Swap`-based `Swapped` transport until the server's transport replaced them.
