//! [`Transport`]: one network's nodes, built from the rows the database
//! stores, and the selection that picks a node for every call; [`Node`],
//! one of them with its state; [`Clients`], the nodes in stored order.
//!
//! Selection, on every call:
//! - a routed method goes to its route's nodes, rotated one per call, then
//!   to the rest in stored order;
//! - anything else goes to the healthy node with the highest tip (each tip
//!   asked at most once per block time), then the other healthy ones in
//!   stored order, a node lagging that tip by more than `confirmations` (or
//!   with no known tip) after every node in sync, then the demoted ones;
//! - a node is demoted by 3 retryable failures in a row or one rate limit,
//!   and goes last until it answers, or until 10 block times (at least
//!   30 s) pass;
//! - a retryable failure, or a node refusing a method it does not serve
//!   (`rpc::Failure::is_unserved`; it is not demoted), moves on to the next
//!   node; any other error object is the answer; once-only work reaches the
//!   first node only;
//! - a broadcast goes to every node.

use std::{
    cmp::Reverse,
    ops::Deref,
    sync::{
        Arc, Mutex, PoisonError, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::ensure;
use base::{CachedInfo, InfoClient, NetworkConfig, NodeInfo};
use futures_util::future::join_all;
use rpc::{Answer, Batch, BoxFuture, Error, ErrorKind, RawJson, Repetition, Request, Retry};

use crate::models::{Endpoint, NodeState, Route};

/// Retryable failures in a row that demote a node.
const DEMOTION: u32 = 3;

/// The least a demoted node waits before it is tried in order again.
const RECOVERY: Duration = Duration::from_secs(30);

/// The longest a node's tip may take: a slower node is not one to pick.
const PROBE: Duration = Duration::from_secs(3);

/// A chain's reader of one node.
type Reader = fn(Arc<dyn rpc::Client>) -> Arc<dyn InfoClient>;

/// `node` read by `I`.
fn reader<I: InfoClient + 'static>(node: Arc<dyn rpc::Client>) -> Arc<dyn InfoClient> {
    Arc::new(I::new(node))
}

/// A network's nodes, built from stored rows, and the selection over them.
/// It is a client itself: each call picks its node.
#[derive(Clone)]
pub(crate) struct Transport(Arc<Nodes>);

/// What a transport holds: its nodes and routes, and how it builds and
/// judges a node.
struct Nodes {
    table: RwLock<Table>,
    reader: Reader,
    /// What every node must serve: the network's chain ID, or its name.
    chain_id: String,
    /// How long a tip is kept.
    block_time: Duration,
    /// How far a node may trail the highest tip and stay in sync.
    lag: u64,
}

/// The current nodes, in stored order, and the routes over them.
#[derive(Default)]
struct Table {
    nodes: Vec<Arc<Node>>,
    routes: Vec<Rotation>,
}

/// One routed method: the positions of its nodes, and where the next call
/// starts.
struct Rotation {
    method: String,
    nodes: Vec<usize>,
    next: AtomicUsize,
}

impl Transport {
    /// No nodes yet. `I` is used only to make each node's reader. `config`
    /// gives the chain ID every node must serve and the block time the tips
    /// are cached for.
    pub(crate) fn new<I: InfoClient + 'static>(config: &NetworkConfig) -> Self {
        let network = &config.network;
        Self(Arc::new(Nodes {
            table: RwLock::default(),
            reader: reader::<I>,
            chain_id: config
                .chain_id
                .clone()
                .unwrap_or_else(|| network.name.clone()),
            block_time: config.block_time,
            lag: config.confirmations,
        }))
    }

    /// Ready clients in place of endpoints (tests), each counted and still
    /// read by `I`.
    #[cfg(test)]
    pub(crate) fn from_clients<I: InfoClient + 'static>(
        config: &NetworkConfig,
        clients: Vec<Arc<dyn rpc::Client>>,
    ) -> Self {
        let transport = Self::new::<I>(config);
        let nodes = clients.into_iter().map(|client| {
            let counted = rpc::Counted::new(client);
            let statistics = counted.statistics();
            Arc::new(transport.0.node(Arc::new(counted), statistics))
        });
        write(&transport.0.table).nodes = nodes.collect();
        transport
    }

    /// The stored nodes and routes: a node whose URL stays keeps its state;
    /// a new one starts empty; a missing one goes. The same call at boot and
    /// on every edit. Asks no node.
    ///
    /// # Errors
    ///
    /// A stored URL that does not build: that node is left out, the others
    /// load.
    pub(crate) fn load(&self, endpoints: &[Endpoint], routes: &[Route]) -> anyhow::Result<()> {
        let old = self.clients();
        let mut nodes = Vec::new();
        let mut ids = Vec::new();
        let mut failed = Vec::new();
        for endpoint in endpoints {
            let kept = old
                .iter()
                .find(|node| node.endpoint() == Some(&endpoint.url));
            let node = match kept {
                Some(node) => node.clone(),
                None => match self.0.open(&endpoint.url) {
                    Ok(node) => Arc::new(node),
                    Err(error) => {
                        failed.push(format!("node {}: {error}", endpoint.id));
                        continue;
                    }
                },
            };
            nodes.push(node);
            ids.push(endpoint.id);
        }
        let routes = routes.iter().filter_map(|route| {
            let nodes: Vec<usize> = route
                .endpoint_ids
                .iter()
                .filter_map(|id| ids.iter().position(|kept| kept == id))
                .collect();
            (!nodes.is_empty()).then(|| Rotation {
                method: route.method.clone(),
                nodes,
                next: AtomicUsize::new(0),
            })
        });
        *write(&self.0.table) = Table {
            nodes,
            routes: routes.collect(),
        };
        ensure!(failed.is_empty(), "{}", failed.join("; "));
        Ok(())
    }

    /// Replaces one node with `endpoint`; it starts with empty state, and
    /// the routes over it stay.
    ///
    /// # Errors
    ///
    /// An invalid `endpoint`, or a node that is no longer here.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "082: no route swaps one node yet")
    )]
    pub(crate) fn replace(&self, node: &Node, endpoint: &rpc::Endpoint) -> anyhow::Result<()> {
        let fresh = Arc::new(self.0.open(endpoint)?);
        let mut table = write(&self.0.table);
        let slot = table
            .nodes
            .iter_mut()
            .find(|held| std::ptr::eq(&***held, node));
        let Some(slot) = slot else {
            anyhow::bail!("the node is no longer this network's");
        };
        *slot = fresh;
        Ok(())
    }

    /// Asks the node at `endpoint` what it serves; fails unless its chain ID
    /// is this network's. Run before storing a new node.
    ///
    /// # Errors
    ///
    /// An invalid endpoint, a node failure, or a node of another chain.
    pub(crate) async fn verify(&self, endpoint: &rpc::Endpoint) -> anyhow::Result<NodeInfo> {
        let info = self.0.open(endpoint)?.info().await?;
        let expected = &self.0.chain_id;
        ensure!(
            info.chain_id == *expected,
            "the node serves {}, not this network's {expected}",
            info.chain_id
        );
        Ok(info)
    }

    /// The current nodes, in stored order.
    pub(crate) fn clients(&self) -> Clients {
        Clients(read(&self.0.table).nodes.clone())
    }

    /// The nodes to try for `method`, in order: see the module.
    async fn candidates(&self, method: Option<&str>) -> Vec<Arc<Node>> {
        let (clients, route) = {
            let table = read(&self.0.table);
            let route = method
                .and_then(|method| table.routes.iter().find(|route| route.method == method))
                .map(Rotation::turn);
            (Clients(table.nodes.clone()), route)
        };
        let Some(route) = route else {
            let ranked = clients.ranked(self.0.lag).await;
            return ranked.into_iter().cloned().collect();
        };
        let rest = (0..clients.len()).filter(|position| !route.contains(position));
        let mut candidates: Vec<Arc<Node>> = route
            .iter()
            .copied()
            .chain(rest)
            .map(|position| clients[position].clone())
            .collect();
        candidates.sort_by_key(|node| !node.healthy());
        candidates
    }

    /// `send` to each candidate for `method` until one answers or fails for
    /// good; once-only work to the first only.
    async fn walk<T>(
        &self,
        method: Option<&str>,
        repetition: Repetition,
        send: impl for<'c> Fn(&'c dyn rpc::Client) -> BoxFuture<'c, Result<T, Error>>,
    ) -> Result<T, Error> {
        let mut outcome = Err(no_node());
        for node in self.candidates(method).await {
            outcome = send(&*node.client).await;
            node.observe(outcome.as_ref().err());
            let retryable = matches!(&outcome, Err(error) if error.is_retryable());
            let unserved = matches!(&outcome, Err(Error::Remote(failure)) if failure.is_unserved());
            if !(retryable || unserved) || repetition == Repetition::Forbidden {
                return outcome;
            }
        }
        outcome
    }

    /// Every node once, concurrently, waiting for all; the first success
    /// answers. Otherwise a transport failure wins over an error object,
    /// because a node whose outcome is unknown may have accepted what the
    /// others refused.
    async fn fan_out(&self, request: Request) -> Result<(), Error> {
        let clients = self.clients();
        let sends = clients
            .iter()
            .map(|node| node.client.broadcast(request.clone()));
        let mut failed = None;
        for outcome in join_all(sends).await {
            match outcome {
                Ok(()) => return Ok(()),
                Err(error @ Error::Transport { .. }) => failed = Some(error),
                Err(refusal) => failed = failed.or(Some(refusal)),
            }
        }
        Err(failed.unwrap_or_else(no_node))
    }
}

impl base::Transport for Transport {
    /// Retry over the selection: the same nodes for the transport's whole
    /// life, a node chosen on every call.
    fn client(&self) -> Arc<dyn rpc::Client> {
        Arc::new(Retry::new(self.clone()))
    }
}

impl rpc::Client for Transport {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(async move {
            let (method, repetition) = (Some(request.method()), request.repetition());
            self.walk(method, repetition, |client| client.request(request.clone()))
                .await
        })
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(async move {
            let (method, repetition) = (batch.method(), batch.repetition());
            self.walk(method, repetition, |client| client.batch(batch.clone()))
                .await
        })
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.fan_out(request))
    }
}

impl Nodes {
    /// The node at `endpoint`; asks it nothing.
    fn open(&self, endpoint: &rpc::Endpoint) -> anyhow::Result<Node> {
        // No sink yet: nothing reads RPC failure events.
        let node = rpc::node(endpoint, Default::default())?;
        let statistics = node.statistics();
        Ok(self.node(Arc::new(node), statistics))
    }

    /// `client` as a node of this network, `statistics` its counters.
    fn node(&self, client: Arc<dyn rpc::Client>, statistics: rpc::Statistics) -> Node {
        Node {
            info: CachedInfo::new((self.reader)(client.clone()), self.block_time),
            client,
            statistics,
            health: Mutex::default(),
            recovery: self.block_time.saturating_mul(10).max(RECOVERY),
        }
    }
}

impl Rotation {
    /// Its nodes, starting one further than the last call did.
    fn turn(&self) -> Vec<usize> {
        let count = self.nodes.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % count;
        (0..count)
            .map(|step| self.nodes[(start + step) % count])
            .collect()
    }
}

/// One node: its client, its cached reader, its state.
pub(crate) struct Node {
    /// `rpc::node(url)`: Http, Throttle, Split, Logged, Counted.
    client: Arc<dyn rpc::Client>,
    /// The chain's reader over `client`, its tip kept for a block time.
    info: CachedInfo,
    /// `client`'s counters.
    statistics: rpc::Statistics,
    health: Mutex<Health>,
    /// How long a demoted node waits before it is tried in order again.
    recovery: Duration,
}

/// A node's failure streak, and since when it is demoted.
#[derive(Default)]
struct Health {
    failures: u32,
    demoted: Option<Instant>,
}

impl Node {
    /// The URL it was built from; `None` for a ready client (tests).
    pub(crate) fn endpoint(&self) -> Option<&rpc::Endpoint> {
        self.client.endpoint()
    }

    /// Its identity, [`rpc::Endpoint::id`].
    pub(crate) fn id(&self) -> Option<String> {
        self.endpoint().map(rpc::Endpoint::id)
    }

    /// What we know about it: last tip, failures, statistics; asks nothing.
    pub(crate) async fn state(&self) -> NodeState {
        let snapshot = self.statistics.snapshot();
        NodeState {
            tip: self.info.last().await,
            failures: lock(&self.health).failures,
            calls: snapshot.attempts,
            failed_calls: snapshot.failures,
            last_failure: snapshot.last_failure,
        }
    }

    /// Asks it now: its chain ID and tip.
    ///
    /// # Errors
    ///
    /// A node failure.
    pub(crate) async fn info(&self) -> anyhow::Result<NodeInfo> {
        self.info.info().await
    }

    /// Whether selection takes it in order: not demoted, or demoted long
    /// enough ago.
    fn healthy(&self) -> bool {
        let health = lock(&self.health);
        let Some(since) = health.demoted else {
            return true;
        };
        since.elapsed() >= self.recovery
    }

    /// Its tip for selection: asked at most once per block time and at most
    /// [`PROBE`] long while healthy, a failed ask counting as a failure; a
    /// demoted node's last one.
    async fn tip(&self) -> Option<u64> {
        if !self.healthy() {
            return self.info.last().await;
        }
        let Ok(Ok(tip)) = tokio::time::timeout(PROBE, self.info.tip()).await else {
            self.failed(false);
            return None;
        };
        Some(tip)
    }

    /// Counts a call's outcome: a success ends the streak; a retryable
    /// failure extends it, a rate limit to a demotion at once; any other
    /// error is the node answering.
    fn observe(&self, error: Option<&Error>) {
        match error {
            None => *lock(&self.health) = Health::default(),
            Some(error) if error.is_retryable() => self.failed(error.is_rate_limited()),
            Some(_) => {}
        }
    }

    fn failed(&self, limited: bool) {
        let mut health = lock(&self.health);
        health.failures = health.failures.saturating_add(1);
        if limited {
            health.failures = health.failures.max(DEMOTION);
        }
        if health.failures >= DEMOTION {
            health.demoted = Some(Instant::now());
        }
    }
}

/// The current nodes, in stored order: a list, plus the questions selection
/// asks of it.
pub(crate) struct Clients(Vec<Arc<Node>>);

impl Deref for Clients {
    type Target = [Arc<Node>];

    fn deref(&self) -> &[Arc<Node>] {
        &self.0
    }
}

impl Clients {
    /// The healthy node with the highest tip; ties and unknown tips: stored
    /// order.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "082: selection ranks every node; tests ask")
    )]
    pub(crate) async fn highest(&self) -> Option<&Arc<Node>> {
        let ranked = self.ranked(u64::MAX).await;
        ranked.into_iter().next().filter(|node| node.healthy())
    }

    /// The node whose id is `id`.
    pub(crate) fn find(&self, id: &str) -> Option<&Arc<Node>> {
        self.iter().find(|node| node.id().as_deref() == Some(id))
    }

    /// Each of `endpoints` with the state of its node; none for one that
    /// did not load.
    pub(crate) async fn states(&self, endpoints: &mut [Endpoint]) {
        for endpoint in endpoints {
            let Some(node) = self.find(&endpoint.url.id()) else {
                continue;
            };
            endpoint.state = Some(node.state().await);
        }
    }

    /// Every node in selection order: the healthy one with the highest tip
    /// (ties and unknown tips: stored order), then the other healthy ones
    /// within `lag` of that tip, then those further behind or with no known
    /// tip, then the demoted ones; each group in stored order.
    async fn ranked(&self, lag: u64) -> Vec<&Arc<Node>> {
        let tips = join_all(self.iter().map(|node| node.tip())).await;
        let healthy: Vec<bool> = self.iter().map(|node| node.healthy()).collect();
        let highest = (0..self.len())
            .filter(|&position| healthy[position])
            .min_by_key(|&position| Reverse(tips[position]));
        let top = highest.and_then(|position| tips[position]);
        let group = |position: usize| match (top, tips[position]) {
            _ if Some(position) == highest => 0,
            _ if !healthy[position] => 3,
            (Some(top), Some(tip)) if top.saturating_sub(tip) <= lag => 1,
            _ => 2,
        };
        let mut order: Vec<usize> = (0..self.len()).collect();
        order.sort_by_key(|&position| group(position));
        order.into_iter().map(|position| &self[position]).collect()
    }
}

/// Why a call reached no node.
fn no_node() -> Error {
    Error::Transport {
        kind: ErrorKind::InvalidConfiguration,
        message: "the network has no RPC node".to_owned(),
    }
}

// Each critical section is one assignment or one clone, so a panic cannot
// leave the state torn: recovering the guard is sound.
fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

fn lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use base::{Chain, Future, InfoClient, Network, NetworkConfig, NodeInfo};
    use rpc::{
        Answer, Batch, BoxFuture, Client as _, Error, ErrorKind, RawJson, Request,
        testing::{Fault, Faults, Reply, Script, Switch},
    };
    use serde_json::json;

    use super::{Clients, Transport};

    /// A node whose tip is its `tip` call.
    struct Tip(Arc<dyn rpc::Client>);

    impl rpc::Client for Tip {
        fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
            self.0.request(request)
        }

        fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
            self.0.batch(batch)
        }
    }

    impl InfoClient for Tip {
        fn new(node: Arc<dyn rpc::Client>) -> Self {
            Self(node)
        }

        fn tip(&self) -> Future<'_, anyhow::Result<u64>> {
            Box::pin(async move {
                let raw = self.0.request(Request::new("tip", json!([]))).await?;
                Ok(raw.deserialize()?)
            })
        }

        fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>> {
            Box::pin(async { anyhow::bail!("not asked") })
        }
    }

    /// A network whose tips are kept a minute and whose nodes may trail
    /// the highest tip by 2.
    fn config() -> NetworkConfig {
        NetworkConfig {
            network: Network::new(Chain::from_static("test"), "net"),
            chain_id: None,
            testnet: true,
            block_time: Duration::from_secs(60),
            confirmations: 2,
            max_depth: None,
            explorer_address: None,
            explorer_transaction: None,
            settings: json!({}),
            assets: Vec::new(),
        }
    }

    /// The transport over `count` scripted nodes, failing on demand.
    fn scripted(count: usize) -> (Transport, Vec<(Script, Switch)>) {
        let (clients, nodes) = (0..count)
            .map(|_| {
                let script = Script::new([]);
                let faults = Faults::new(script.clone());
                let switch = faults.switch();
                let client: Arc<dyn rpc::Client> = Arc::new(faults);
                (client, (script, switch))
            })
            .unzip();
        (Transport::from_clients::<Tip>(&config(), clients), nodes)
    }

    /// [`scripted`] nodes, each first telling its tip of `tips`.
    fn transport(tips: &[u64]) -> (Transport, Vec<(Script, Switch)>) {
        let (transport, nodes) = scripted(tips.len());
        for ((script, _), tip) in nodes.iter().zip(tips) {
            script.push([Reply::ok("tip", json!(tip))]);
        }
        (transport, nodes)
    }

    async fn ask(transport: &Transport, method: &str) -> Result<u64, Error> {
        let raw = transport.request(Request::new(method, json!([]))).await?;
        raw.deserialize()
    }

    fn position(clients: &Clients, node: Option<&Arc<super::Node>>) -> Option<usize> {
        let node = node?;
        clients.iter().position(|held| Arc::ptr_eq(held, node))
    }

    #[tokio::test]
    async fn the_highest_tip_serves_and_a_tie_keeps_the_stored_order() {
        let (highest, nodes) = transport(&[5, 9]);
        nodes[1].0.push([Reply::ok("m", json!(1))]);
        assert_eq!(ask(&highest, "m").await.unwrap(), 1);
        let clients = highest.clients();
        assert_eq!(position(&clients, clients.highest().await), Some(1));
        assert_eq!(clients[1].state().await.tip, Some(9));

        let (tie, ties) = transport(&[9, 9]);
        ties[0].0.push([Reply::ok("m", json!(0))]);
        assert_eq!(ask(&tie, "m").await.unwrap(), 0);
        let clients = tie.clients();
        assert_eq!(position(&clients, clients.highest().await), Some(0));
        for (script, _) in nodes.iter().chain(&ties) {
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn a_retryable_failure_moves_on_and_a_lagging_node_goes_after_those_in_sync() {
        let (transport, nodes) = transport(&[10, 5, 9]);
        nodes[0]
            .1
            .fail_next(Some("m"), Fault::Transport(ErrorKind::Unavailable));
        nodes[2].0.push([Reply::ok("m", json!(2))]);
        assert_eq!(
            ask(&transport, "m").await.unwrap(),
            2,
            "9 is in sync, 5 is not"
        );

        nodes[0]
            .0
            .push([Reply::failure("m", -32_000, "execution reverted")]);
        let error = ask(&transport, "m").await.unwrap_err();
        assert!(
            matches!(error, Error::Remote(_)),
            "an error object is the answer"
        );
        for (script, _) in &nodes {
            script.exhausted();
        }
    }

    /// X2: Tatum's free plan answers `getBalance` and
    /// `getTokenAccountsByOwner` with an error object (`-16401`, HTTP 200);
    /// taken as the answer, every balance read routed to it failed.
    #[tokio::test]
    async fn a_method_the_node_does_not_serve_moves_on_without_demoting_it() {
        let (transport, nodes) = transport(&[10, 9]);
        let paid = "Method 'm' is available for paid plans only.";
        nodes[0].0.push([Reply::failure("m", -16_401, paid)]);
        nodes[1].0.push([Reply::ok("m", json!(1))]);
        assert_eq!(ask(&transport, "m").await.unwrap(), 1);
        let clients = transport.clients();
        assert_eq!(clients[0].state().await.failures, 0, "not demoted");
        assert_eq!(position(&clients, clients.highest().await), Some(0));
        for (script, _) in &nodes {
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn failures_demote_and_an_answer_or_the_recovery_restores() {
        let (transport, nodes) = transport(&[9, 5]);
        let (first, second) = (&nodes[0], &nodes[1]);
        first.0.push([Reply::ok("m", json!(0))]);
        assert_eq!(ask(&transport, "m").await.unwrap(), 0);
        first.1.down(true);
        second.0.push((0..4).map(|_| Reply::ok("m", json!(1))));
        for _ in 0..3 {
            assert_eq!(ask(&transport, "m").await.unwrap(), 1);
        }
        let asked = first.1.requests();
        assert_eq!(ask(&transport, "m").await.unwrap(), 1);
        assert_eq!(first.1.requests(), asked, "a demoted node goes last");
        let clients = transport.clients();
        assert_eq!(clients[0].state().await.failures, 3);
        assert_eq!(position(&clients, clients.highest().await), Some(1));

        let since = Instant::now().checked_sub(clients[0].recovery).unwrap();
        super::lock(&clients[0].health).demoted = Some(since);
        assert!(clients[0].healthy(), "back in order after the recovery");
        super::lock(&clients[0].health).demoted = Some(Instant::now());

        first.1.down(false);
        second.1.down(true);
        first.0.push([Reply::ok("m", json!(0))]);
        assert_eq!(
            ask(&transport, "m").await.unwrap(),
            0,
            "the demoted node, last"
        );
        assert_eq!(
            clients[0].state().await.failures,
            0,
            "its answer ends the streak"
        );
        for (script, _) in &nodes {
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn a_rate_limit_demotes_at_once() {
        let (transport, nodes) = transport(&[9, 5]);
        let limit = Fault::Remote(-32_005, "Rate limit exceeded".to_owned());
        nodes[0].1.fail_next(Some("m"), limit);
        nodes[1]
            .0
            .push([Reply::ok("m", json!(1)), Reply::ok("m", json!(1))]);
        assert_eq!(ask(&transport, "m").await.unwrap(), 1);
        assert_eq!(ask(&transport, "m").await.unwrap(), 1);
        let clients = transport.clients();
        assert_eq!(clients[0].state().await.failures, 3);
        for (script, _) in &nodes {
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn a_routed_method_rotates_its_nodes_and_once_only_work_reaches_one() {
        let (transport, nodes) = scripted(3);
        // Over ready clients the route is set as `load` sets it from ids.
        super::write(&transport.0.table).routes = vec![super::Rotation {
            method: "m".to_owned(),
            nodes: vec![1, 2],
            next: std::sync::atomic::AtomicUsize::new(0),
        }];
        nodes[1]
            .0
            .push([Reply::ok("m", json!(1)), Reply::ok("m", json!(1))]);
        nodes[2]
            .0
            .push([Reply::ok("m", json!(2)), Reply::ok("m", json!(2))]);
        let mut answers = Vec::new();
        for _ in 0..4 {
            answers.push(ask(&transport, "m").await.unwrap());
        }
        assert_eq!(answers, [1, 2, 1, 2]);

        nodes[1]
            .1
            .fail_next(None, Fault::Transport(ErrorKind::Unavailable));
        nodes[2].0.push([Reply::ok("m", json!(2))]);
        assert_eq!(
            ask(&transport, "m").await.unwrap(),
            2,
            "moves on in the route"
        );

        nodes[2]
            .1
            .fail_next(None, Fault::Transport(ErrorKind::Unavailable));
        let asked = nodes[1].1.requests();
        let once = transport.request(Request::once("m", json!([]))).await;
        assert_eq!(once.unwrap_err().kind(), Some(ErrorKind::Unavailable));
        assert_eq!(
            nodes[1].1.requests(),
            asked,
            "once-only work reaches one node"
        );
        for (script, _) in &nodes {
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn a_broadcast_reaches_every_node_once() {
        let (transport, nodes) = scripted(2);
        for (script, _) in &nodes {
            script.push([Reply::ok("send", json!("id"))]);
        }
        let sent = transport.broadcast(Request::once("send", json!([])));
        sent.await.unwrap();
        for (_, switch) in &nodes {
            assert_eq!(switch.calls("send"), 1);
        }
    }

    #[tokio::test]
    async fn load_keeps_a_node_whose_url_stays_and_replace_starts_one_empty() {
        let transport = Transport::new::<Tip>(&config());
        let endpoint = |id: i64| crate::models::Endpoint {
            id,
            url: rpc::Endpoint::new(format!("http://node-{id}.invalid/")),
            state: None,
        };
        transport.load(&[endpoint(1), endpoint(2)], &[]).unwrap();
        let before = transport.clients();
        super::lock(&before[0].health).failures = 2;
        transport.load(&[endpoint(2), endpoint(1)], &[]).unwrap();
        let after = transport.clients();
        assert!(Arc::ptr_eq(&before[0], &after[1]), "kept, now second");
        assert_eq!(after[1].state().await.failures, 2);
        assert!(after.find("node-2.invalid/").is_some());

        let fresh = rpc::Endpoint::new("http://node-3.invalid/");
        transport.replace(&after[1], &fresh).unwrap();
        let replaced = transport.clients();
        assert_eq!(replaced[1].endpoint(), Some(&fresh));
        assert_eq!(replaced[1].state().await.failures, 0);
        assert!(
            transport.replace(&after[1], &fresh).is_err(),
            "already gone"
        );

        let bad = crate::models::Endpoint {
            id: 4,
            url: rpc::Endpoint::new("ftp://node.invalid/"),
            state: None,
        };
        assert!(transport.load(&[endpoint(1), bad], &[]).is_err());
        assert_eq!(transport.clients().len(), 1, "the rest load");
    }
}
