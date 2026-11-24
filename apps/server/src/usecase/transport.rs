//! [`Transport`]: one network's nodes, built from the rows the database
//! stores, and the selection that picks a node for every call. Each node,
//! its stack and its state are `nodes`' ([`Node`], [`Nodes`]).
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
//! - a broadcast goes to every node;
//! - a disabled node is not loaded, and routes skip it; a node whose
//!   switch turns off since it was loaded is left out at once (and a call
//!   it had moves on);
//! - every call runs in a `network` span, each node's call in a `node`
//!   span (`network`, `node`: its stored id), each made per call under the
//!   caller's (a job's); every request of a node is logged (`rpc.http`);
//!   each move to the next node is logged (`rpc.failover`, debug); a node's
//!   demotion and its end (`node.demoted`, `node.restored`);
//! - the network losing its last node is logged as `network.unserved`,
//!   error, and getting one back as `network.served`, info: checked on
//!   every load and after every failed call.
//!
//! A node's tip and failure streak live here, in memory; whether a stored
//! node is enabled, and its counts, live in the database: its `Guard` and
//! wrong-chain probe turn it off there (its [`NodeSwitch`]), its `Counter`
//! counts there.

use std::sync::{
    Arc, PoisonError, RwLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use anyhow::ensure;
use base::{InfoClient, NetworkConfig, NodeInfo};
use futures_util::future::join_all;
use rpc::{Answer, Batch, BoxFuture, Error, ErrorKind, RawJson, Repetition, Request, Retry};

use super::nodes::{Node, NodeFactory, Nodes, reader};
use crate::{
    db::Db,
    models::{Endpoint, Endpoints, Route},
};

/// Why a call reached no node, an index job idles and a payment waits.
pub(crate) const NO_ENABLED_ENDPOINT: &str = "the network has no enabled RPC endpoint";

/// A network's nodes, built from stored rows, and the selection over them.
/// It is a client itself: each call picks its node.
#[derive(Clone)]
pub(crate) struct Transport(Arc<Shared>);

/// What a transport holds: its nodes and routes, and how it builds a node.
struct Shared {
    table: RwLock<Table>,
    /// Builds each node.
    factory: NodeFactory,
    /// How far a node may trail the highest tip and stay in sync.
    lag: u64,
    /// Whether it had a node to call when last checked ([`Transport::check_served`]).
    served: AtomicBool,
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
    /// are cached for; each stored node is turned off and counted in `db`.
    pub(crate) fn new<I: InfoClient + 'static>(config: &NetworkConfig, db: Db) -> Self {
        Self::with::<I>(config, Some(db))
    }

    /// [`Self::new`], its stored nodes neither turned off nor counted when
    /// `db` is `None`.
    fn with<I: InfoClient + 'static>(config: &NetworkConfig, db: Option<Db>) -> Self {
        let network = &config.network;
        Self(Arc::new(Shared {
            table: RwLock::default(),
            factory: NodeFactory {
                reader: reader::<I>,
                chain_id: config
                    .chain_id
                    .clone()
                    .unwrap_or_else(|| network.name.clone()),
                block_time: config.block_time,
                db,
                label: super::network_label(network),
            },
            lag: config.confirmations,
            served: AtomicBool::new(true),
        }))
    }

    /// Test clients in place of endpoints, each built into a node as a
    /// stored node's HTTP client is, not stored (neither guarded nor
    /// counted), read by `I`; stored nodes loaded later are, in `db`.
    #[cfg(test)]
    pub(crate) fn from_clients<I: InfoClient + 'static>(
        config: &NetworkConfig,
        clients: Vec<Arc<dyn rpc::Client>>,
        db: Option<Db>,
    ) -> Self {
        let transport = Self::with::<I>(config, db);
        let url = rpc::Endpoint::new("http://test.invalid/").parse();
        let url = url.expect("a test URL");
        let nodes = clients.into_iter().map(|client| {
            let node = transport.0.factory.build(client, &url, None);
            Arc::new(node.expect("a test node"))
        });
        write(&transport.0.table).nodes = nodes.collect();
        transport
    }

    /// The stored enabled nodes and routes: a node whose URL stays keeps
    /// its state; a new one starts afresh; a missing or disabled one goes,
    /// and routes skip it. The same call at boot and on every edit. Asks no
    /// node.
    ///
    /// # Errors
    ///
    /// A stored URL that does not build: that node is left out, the others
    /// load.
    pub(crate) fn load(&self, endpoints: &Endpoints, routes: &[Route]) -> anyhow::Result<()> {
        let old = self.clients();
        let mut nodes = Vec::new();
        let mut ids = Vec::new();
        let mut failed = Vec::new();
        let open = |endpoint: &Endpoint| -> anyhow::Result<Node> {
            let url = endpoint.url.parse()?;
            let http = Arc::new(rpc::Http::new(&url, endpoint.url.clone())?);
            self.0.factory.build(http, &url, Some(endpoint.id))
        };
        for endpoint in endpoints
            .iter()
            .filter(|endpoint| endpoint.disabled.is_none())
        {
            let kept = old
                .iter()
                .find(|node| node.endpoint() == Some(&endpoint.url));
            let node = match kept {
                Some(node) => node.clone(),
                None => match open(endpoint) {
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
        self.check_served();
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
        let url = endpoint.parse()?;
        let http = Arc::new(rpc::Http::new(&url, endpoint.clone())?);
        let fresh = Arc::new(self.0.factory.build(http, &url, node.endpoint_id)?);
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
        let url = endpoint.parse()?;
        let http = Arc::new(rpc::Http::new(&url, endpoint.clone())?);
        let info = self.0.factory.build(http, &url, None)?.info().await?;
        let expected = &self.0.factory.chain_id;
        ensure!(
            info.chain_id == *expected,
            "the node serves {}, not this network's {expected}",
            info.chain_id
        );
        Ok(info)
    }

    /// The highest tip its nodes last told, kept in memory; `None` before
    /// any told one. Asks no node.
    pub(crate) async fn known_tip(&self) -> Option<u64> {
        let nodes = self.clients();
        let tips = join_all(nodes.iter().map(|node| node.known_tip())).await;
        tips.into_iter().flatten().max()
    }

    /// Whether it has no node to call: every stored one disabled or off.
    pub(crate) fn has_no_node(&self) -> bool {
        self.clients().is_empty()
    }

    /// Logs the network losing its last node (`network.unserved`, error) or
    /// getting one back (`network.served`, info), once per change.
    fn check_served(&self) {
        let served = !self.has_no_node();
        if self.0.served.swap(served, Ordering::Relaxed) == served {
            return;
        }
        let network = &self.0.factory.label;
        if served {
            tracing::info!(target: "network.served", network, "Network has an RPC node again");
        } else {
            tracing::error!(target: "network.unserved", network, "Network has no enabled RPC node");
        }
    }

    /// The current nodes, in stored order, without those turned off since
    /// they were loaded.
    pub(crate) fn clients(&self) -> Nodes {
        let nodes = read(&self.0.table).nodes.clone();
        Nodes(nodes.into_iter().filter(|node| !node.is_off()).collect())
    }

    /// The nodes to try for `method`, in order: see the module.
    async fn candidates(&self, method: Option<&str>) -> Vec<Arc<Node>> {
        let (clients, route) = {
            let table = read(&self.0.table);
            let route = method
                .and_then(|method| table.routes.iter().find(|route| route.method == method))
                .map(Rotation::turn);
            (Nodes(table.nodes.clone()), route)
        };
        let Some(route) = route else {
            let clients = self.clients();
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
        candidates.retain(|node| !node.is_off());
        candidates.sort_by_key(|node| !node.healthy());
        candidates
    }

    /// `send` to each candidate for `method` until one answers or fails for
    /// good; once-only work to the first it reaches only.
    async fn walk<T>(
        &self,
        method: Option<&str>,
        repetition: Repetition,
        send: impl for<'c> Fn(&'c dyn rpc::Client) -> BoxFuture<'c, Result<T, Error>>,
    ) -> Result<T, Error> {
        let candidates = self.candidates(method).await;
        let method = method.unwrap_or("batch");
        let mut outcome = Err(no_node());
        let mut from: Option<&Arc<Node>> = None;
        for node in &candidates {
            if let (Some(from), Err(error)) = (from, &outcome) {
                self.failover(method, from, node, error);
            }
            outcome = send(node.client()).await;
            node.observe(outcome.as_ref().err());
            let retryable = matches!(&outcome, Err(error) if error.is_retryable());
            let unserved = matches!(&outcome, Err(Error::Remote(failure)) if failure.is_unserved());
            let gone = outcome.is_err() && node.is_off();
            let walks = repetition == Repetition::Permitted && (retryable || unserved || gone);
            if !walks {
                break;
            }
            from = Some(node);
        }
        if outcome.is_err() {
            self.check_served();
        }
        outcome
    }

    /// Logs a call of `method` moving on from node `from`, failed with
    /// `error`, to node `to`: `rpc.failover`, debug.
    fn failover(&self, method: &str, from: &Node, to: &Node, error: &Error) {
        let (from, to, reason) = (from.label(), to.label(), error.label());
        tracing::debug!(
            target: "rpc.failover",
            method, from, to, reason,
            "RPC call moved on to the next node"
        );
    }

    /// Every node once, concurrently, waiting for all; the first success
    /// answers. Otherwise a transport failure wins over an error object,
    /// because a node whose outcome is unknown may have accepted what the
    /// others refused.
    async fn fan_out(&self, request: Request) -> Result<(), Error> {
        let clients = self.clients();
        let sends = clients
            .iter()
            .map(|node| node.client().broadcast(request.clone()));
        let mut failed = None;
        for outcome in join_all(sends).await {
            match outcome {
                Ok(()) => return Ok(()),
                Err(error @ Error::Transport { .. }) => failed = Some(error),
                Err(refusal) => failed = failed.or(Some(refusal)),
            }
        }
        self.check_served();
        Err(failed.unwrap_or_else(no_node))
    }
}

/// What a container of server transports answers about a network's nodes.
pub(crate) trait Served {
    /// Whether `network` has a node to call: registered, with at least one
    /// enabled node not turned off since it was loaded.
    fn has_node(&self, network: &base::Network) -> bool;
}

impl Served for base::Container<Transport> {
    fn has_node(&self, network: &base::Network) -> bool {
        self.transport(network)
            .is_ok_and(|transport| !transport.has_no_node())
    }
}

impl base::Transport for Transport {
    /// Retry over the selection, in the network's span: the same nodes for
    /// the transport's whole life, a node chosen on every call.
    fn client(&self) -> Arc<dyn rpc::Client> {
        let retry = Retry::new(self.clone());
        let label = self.0.factory.label.clone();
        let span = Arc::new(move || tracing::info_span!("network", network = %label));
        Arc::new(rpc::Spanned::new(retry, span))
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

/// Why a call reached no node.
fn no_node() -> Error {
    Error::Transport {
        kind: ErrorKind::InvalidConfiguration,
        message: NO_ENABLED_ENDPOINT.to_owned(),
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

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use base::{Chain, Network, NetworkConfig, testing::TipInfo};
    use rpc::{
        Client as _, Error, ErrorKind, Request,
        testing::{Fault, Faults, Reply, Script, Switch},
    };
    use serde_json::json;

    use super::{Nodes, Transport};
    use crate::models::{Endpoint, EndpointFailure, Endpoints};
    use crate::usecase::nodes::lock;

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
        (
            Transport::from_clients::<TipInfo>(&config(), clients, None),
            nodes,
        )
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

    fn position(clients: &Nodes, node: Option<&Arc<super::Node>>) -> Option<usize> {
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
        assert_eq!(clients[1].info.last().await, Some(9));

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
        assert_eq!(lock(&clients[0].health).failures, 0, "not demoted");
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
        assert_eq!(lock(&clients[0].health).failures, 3);
        assert_eq!(position(&clients, clients.highest().await), Some(1));

        let since = Instant::now().checked_sub(clients[0].recovery).unwrap();
        lock(&clients[0].health).demoted = Some(since);
        assert!(clients[0].healthy(), "back in order after the recovery");
        lock(&clients[0].health).demoted = Some(Instant::now());

        first.1.down(false);
        second.1.down(true);
        first.0.push([Reply::ok("m", json!(0))]);
        assert_eq!(
            ask(&transport, "m").await.unwrap(),
            0,
            "the demoted node, last"
        );
        assert_eq!(
            lock(&clients[0].health).failures,
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
        assert_eq!(lock(&clients[0].health).failures, 3);
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
        let transport = Transport::from_clients::<TipInfo>(&config(), Vec::new(), None);
        transport
            .load(&nodes([endpoint(1), endpoint(2)]), &[])
            .unwrap();
        let before = transport.clients();
        lock(&before[0].health).failures = 2;
        transport
            .load(&nodes([endpoint(2), endpoint(1)]), &[])
            .unwrap();
        let after = transport.clients();
        assert!(Arc::ptr_eq(&before[0], &after[1]), "kept, now second");
        assert_eq!(lock(&after[1].health).failures, 2);
        assert!(after.find("node-2.invalid/").is_some());

        let fresh = rpc::Endpoint::new("http://node-3.invalid/");
        transport.replace(&after[1], &fresh).unwrap();
        let replaced = transport.clients();
        assert_eq!(replaced[1].endpoint(), Some(&fresh));
        assert_eq!(lock(&replaced[1].health).failures, 0);
        assert!(
            transport.replace(&after[1], &fresh).is_err(),
            "already gone"
        );

        let bad = Endpoint {
            url: rpc::Endpoint::new("ftp://node.invalid/"),
            ..endpoint(4)
        };
        assert!(transport.load(&nodes([endpoint(1), bad]), &[]).is_err());
        assert_eq!(transport.clients().len(), 1, "the rest load");
    }

    /// Stored node `id` at `http://node-{id}.invalid/`, enabled.
    fn endpoint(id: i64) -> Endpoint {
        Endpoint {
            id,
            url: rpc::Endpoint::new(format!("http://node-{id}.invalid/")),
            position: id,
            stats: crate::models::EndpointStats {
                endpoint_id: id,
                calls: 0,
                failed_calls: 0,
                updated_at: 0,
            },
            disabled: None,
        }
    }

    /// `endpoints`, as a network's.
    fn nodes<const N: usize>(endpoints: [Endpoint; N]) -> Endpoints {
        Endpoints::from(Vec::from(endpoints))
    }

    #[tokio::test]
    async fn a_disabled_node_is_left_out_and_its_routes_skip_it() {
        let transport = Transport::from_clients::<TipInfo>(&config(), Vec::new(), None);
        let disabled = Endpoint {
            disabled: Some(EndpointFailure::manual(2, 0, "flaky".to_owned())),
            ..endpoint(2)
        };
        let route = |ids: Vec<i64>| crate::models::Route {
            method: "m".to_owned(),
            endpoint_ids: ids,
        };
        let endpoints = nodes([endpoint(1), disabled.clone()]);
        transport.load(&endpoints, &[route(vec![2])]).unwrap();
        let clients = transport.clients();
        assert_eq!(clients.len(), 1);
        assert!(clients.find("node-1.invalid/").is_some());
        assert!(
            clients.find("node-2.invalid/").is_none(),
            "disabled: left out"
        );
        assert!(
            super::read(&transport.0.table).routes.is_empty(),
            "a route over disabled nodes only is skipped"
        );

        transport.load(&nodes([disabled]), &[]).unwrap();
        assert!(transport.clients().is_empty());
        let error = ask(&transport, "m").await.unwrap_err();
        assert!(
            error.to_string().contains(super::NO_ENABLED_ENDPOINT),
            "{error}"
        );
    }
}
