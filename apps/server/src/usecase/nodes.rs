//! A network's RPC nodes, one at a time: how one is built ([`NodeFactory::build`],
//! its decorator stack), [`Node`] with its in-memory health, [`Nodes`] in
//! stored order with the questions selection asks, and a stored node's
//! [`NodeSwitch`] with the failures that turn it off ([`unrecoverable`]).

use std::{
    cmp::Reverse,
    ops::Deref,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use base::{CachedInfo, InfoClient, NodeInfo};
use futures_util::future::join_all;
use rpc::{Error, ErrorKind};
use tracing::Instrument as _;

use super::{
    decorators::{Counter, Guard},
    now,
};
use crate::{
    db::Db,
    models::{EndpointFailure, endpoint_failure},
};

/// Retryable failures in a row that demote a node.
const DEMOTION: u32 = 3;

/// The least a demoted node waits before it is tried in order again.
const RECOVERY: Duration = Duration::from_secs(30);

/// The longest a node's tip may take: a slower node is not one to pick.
const PROBE: Duration = Duration::from_secs(3);

/// A chain's reader of one node.
pub(super) type Reader = fn(Arc<dyn rpc::Client>) -> Arc<dyn InfoClient>;

/// `node` read by `I`.
pub(super) fn reader<I: InfoClient + 'static>(node: Arc<dyn rpc::Client>) -> Arc<dyn InfoClient> {
    Arc::new(I::new(node))
}

/// Builds a network's nodes ([`Self::build`]): what is the same for each of them.
pub(super) struct NodeFactory {
    /// The chain's reader of one node.
    pub(super) reader: Reader,
    /// What every node must serve: the network's chain ID, or its name.
    pub(super) chain_id: String,
    /// How long a tip is kept.
    pub(super) block_time: Duration,
    /// Where its stored nodes are turned off and counted; `None`: neither
    /// (tests).
    pub(super) db: Option<Db>,
    /// The network as events name it: `chain/name`.
    pub(super) label: String,
}

impl NodeFactory {
    /// The node of this network around `inner` (its HTTP client, or a
    /// test's), stored as `endpoint_id` (`None`: not stored); asks it nothing.
    pub(super) fn build(
        &self,
        inner: Arc<dyn rpc::Client>,
        url: &rpc::Url,
        endpoint_id: Option<i64>,
    ) -> anyhow::Result<Node> {
        let stored = endpoint_id.zip(self.db.clone());
        let switch = stored.clone().map(|(id, db)| NodeSwitch::new(db, id));

        // 1. The node itself: `inner` (its HTTP client, or a test's).
        let client = inner;

        // 2. Traced, in the node's context (`network=chain/name`, `node`:
        //    its stored id, `new` when not stored; `endpoint`: its host, never
        //    its path or query, where keys live): one `rpc.http` event per
        //    request, exactly as sent, seeing what Guard and Counter see.
        let id = endpoint_id.map_or_else(|| "new".to_owned(), |id| id.to_string());
        let network = self.label.clone();
        let host = url.node.host_str().unwrap_or_default().to_owned();
        let span: rpc::MakeSpan = Arc::new(
            move || tracing::info_span!("node", network = %network, node = %id, endpoint = %host),
        );
        let client: Arc<dyn rpc::Client> = Arc::new(rpc::Traced::new(client, span.clone()));

        // 3. Guard (stored nodes only): refuses once the switch is off, before
        //    a refused call is paced or counted; turns it off on an answer the
        //    node cannot recover from (401, 403, 404, 5xx, not on its plan).
        let client: Arc<dyn rpc::Client> = match &switch {
            Some(switch) => Arc::new(Guard::new(client, switch.clone())),
            None => client,
        };

        // 4. Counter (stored nodes only): counts each request sent, a refusal
        //    not, in `network_endpoint_stats`.
        let client: Arc<dyn rpc::Client> = match stored {
            Some((id, db)) => Arc::new(Counter::new(client, db, id)),
            None => client,
        };

        // 5. Throttle: paces each request to `rpc.rps`; its wait stays outside
        //    every timing and timeout below.
        let client = rpc::Throttle::new(client, rpc::ThrottleParams::try_from(url)?.rps);

        // 6. Split: a batch sent in requests of at most `rpc.max_batch`.
        let client = rpc::Split::new(client, rpc::SplitParams::try_from(url)?.max_batch);

        // 7. Spanned, outermost: the same node context for the layers above
        //    Traced (Throttle's waits, a switch turned off).
        let client: Arc<dyn rpc::Client> = Arc::new(rpc::Spanned::new(client, span));

        Ok(Node {
            info: CachedInfo::new((self.reader)(client.clone()), self.block_time),
            client,
            health: Mutex::new(Health::default()),
            recovery: self.block_time.saturating_mul(10).max(RECOVERY),
            endpoint_id,
            switch,
            chain_id: self.chain_id.clone(),
            chain_checked: AtomicBool::new(false),
        })
    }
}

/// One node: its client, its cached reader, its state.
pub(crate) struct Node {
    /// Its stack ([`NodeFactory::build`]).
    client: Arc<dyn rpc::Client>,
    /// The chain's reader over `client`, its tip kept for a block time.
    pub(super) info: CachedInfo,
    pub(super) health: Mutex<Health>,
    /// How long a demoted node waits before it is tried in order again.
    pub(super) recovery: Duration,
    /// Its stored id; `None`: not stored.
    pub(super) endpoint_id: Option<i64>,
    /// What turns it off, its `Guard`'s; `None`: not stored.
    switch: Option<Arc<NodeSwitch>>,
    /// The chain it must serve, the network's.
    chain_id: String,
    /// Whether it told its chain since it was loaded.
    chain_checked: AtomicBool,
}

/// A node's failure streak, and since when it is demoted. A demotion is
/// logged as `node.demoted` (`node reason streak`), warn; the answer that
/// ends it as `node.restored` (`node`), info.
#[derive(Default)]
pub(super) struct Health {
    pub(super) failures: u32,
    pub(super) demoted: Option<Instant>,
}

impl Node {
    /// Its stack, as calls reach it.
    pub(super) fn client(&self) -> &dyn rpc::Client {
        &*self.client
    }

    /// The URL it was built from; `None` for a ready client (tests).
    pub(crate) fn endpoint(&self) -> Option<&rpc::Endpoint> {
        self.client.endpoint()
    }

    /// Its identity, [`rpc::Endpoint::id`].
    #[cfg(test)]
    pub(crate) fn id(&self) -> Option<String> {
        self.endpoint().map(rpc::Endpoint::id)
    }

    /// What logs name it: its stored id, never its URL; `-` for none.
    pub(super) fn label(&self) -> String {
        self.endpoint_id
            .map_or_else(|| "-".to_owned(), |id| id.to_string())
    }

    /// Whether its switch turned off since it was loaded.
    pub(super) fn is_off(&self) -> bool {
        self.switch.as_ref().is_some_and(|switch| switch.is_off())
    }

    /// The last tip it told, however old; `None` before the first. Asks
    /// nothing.
    pub(super) async fn known_tip(&self) -> Option<u64> {
        self.info.last().await
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
    pub(super) fn healthy(&self) -> bool {
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
        self.check_chain().await;
        let tip = match tokio::time::timeout(PROBE, self.info.tip()).await {
            Ok(Ok(tip)) => tip,
            Ok(Err(_)) => {
                self.failed(false, "tip probe failed");
                return None;
            }
            Err(_) => {
                self.failed(false, "tip probe timed out");
                return None;
            }
        };
        Some(tip)
    }

    /// Counts a call's outcome: a success ends the streak; a retryable
    /// failure extends it, a rate limit to a demotion at once; any other
    /// error is the node answering.
    pub(super) fn observe(&self, error: Option<&Error>) {
        match error {
            None => {
                let health = std::mem::take(&mut *lock(&self.health));
                if health.demoted.is_some() {
                    let node = self.label();
                    tracing::info!(target: "node.restored", node, "RPC node restored");
                }
            }
            Some(error) if error.is_retryable() => {
                self.failed(error.is_rate_limited(), &error.label());
            }
            Some(_) => {}
        }
    }

    /// A stored node, until it tells its chain since it was loaded, asked
    /// it at most [`PROBE`] long: another than the network's turns it off
    /// (`wrong_chain`, through its [`NodeSwitch`]). One not stored is not
    /// asked.
    async fn check_chain(&self) {
        let (Some(id), Some(switch)) = (self.endpoint_id, &self.switch) else {
            return;
        };
        if self.chain_checked.load(Ordering::Relaxed) {
            return;
        }
        let Ok(Ok(info)) = tokio::time::timeout(PROBE, self.info.info()).await else {
            return;
        };
        self.chain_checked.store(true, Ordering::Relaxed);
        if info.chain_id != self.chain_id {
            let failure = EndpointFailure::wrong_chain(id, now(), &info.chain_id, &self.chain_id);
            switch.turn_off(failure).await;
        }
    }

    /// One more retryable failure for `reason`; a rate limit (`limited`)
    /// demotes at once.
    fn failed(&self, limited: bool, reason: &str) {
        let mut health = lock(&self.health);
        health.failures = health.failures.saturating_add(1);
        if limited {
            health.failures = health.failures.max(DEMOTION);
        }
        if health.failures < DEMOTION {
            return;
        }
        let demoted = health.demoted.replace(Instant::now());
        let streak = health.failures;
        drop(health);
        if demoted.is_none() {
            let node = self.label();
            tracing::warn!(target: "node.demoted", node, reason, streak, "RPC node demoted");
        }
    }
}

/// The current nodes, in stored order: a list, plus the questions selection
/// asks of it.
pub(crate) struct Nodes(pub(super) Vec<Arc<Node>>);

impl Deref for Nodes {
    type Target = [Arc<Node>];

    fn deref(&self) -> &[Arc<Node>] {
        &self.0
    }
}

impl Nodes {
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
    #[cfg(test)]
    pub(crate) fn find(&self, id: &str) -> Option<&Arc<Node>> {
        self.iter().find(|node| node.id().as_deref() == Some(id))
    }

    /// Every node in selection order: the healthy one with the highest tip
    /// (ties and unknown tips: stored order), then the other healthy ones
    /// within `lag` of that tip, then those further behind or with no known
    /// tip, then the demoted ones; each group in stored order. The tips are
    /// asked in a `node.rank` span.
    pub(super) async fn ranked(&self, lag: u64) -> Vec<&Arc<Node>> {
        let tips = join_all(self.iter().map(|node| node.tip()));
        let tips = tips.instrument(tracing::info_span!("node.rank")).await;
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

pub(super) fn lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

/// HTTP statuses, and error-object codes, a node cannot recover from: its
/// key refused (401, 403), its path unknown (404).
const UNRECOVERABLE: [i64; 3] = [401, 403, 404];

/// What a provider answers a method its plan does not include with (Tatum's
/// `-16401`, "…is available for paid plans only…").
const NOT_ON_PLAN: &str = "paid plans only";

/// `error`, failed by node `endpoint_id`, as a failure it is turned off
/// for: an HTTP status of [`UNRECOVERABLE`] or 500 and above (the node is
/// broken or overloaded: other nodes take its calls), an error object's code
/// of [`UNRECOVERABLE`], or a method refused as not on the node's plan.
pub(crate) fn unrecoverable(endpoint_id: i64, error: &Error) -> Option<EndpointFailure> {
    let message = match error {
        Error::Transport {
            kind: ErrorKind::HttpStatus(status),
            ..
        } if *status >= 500 || UNRECOVERABLE.contains(&i64::from(*status)) => {
            format!("the node answered HTTP {status}")
        }
        Error::Remote(failure) if UNRECOVERABLE.contains(&failure.code) => {
            format!("the node answered error code {}", failure.code)
        }
        Error::Remote(failure) if failure.message.contains(NOT_ON_PLAN) => format!(
            "the node refused a method as not on its plan (error code {})",
            failure.code
        ),
        _ => return None,
    };
    Some(EndpointFailure::new(
        endpoint_id,
        now(),
        error.label(),
        message,
    ))
}

/// One stored node, on until turned off: off at once here, and stored off
/// (a `network_endpoint_failure` row) once. The transport leaves it out
/// from then on, and does not load it again.
pub(crate) struct NodeSwitch {
    db: Db,
    endpoint_id: i64,
    off: AtomicBool,
}

impl NodeSwitch {
    /// Stored node `endpoint_id`, on, turned off in `db`.
    pub(crate) fn new(db: Db, endpoint_id: i64) -> Arc<Self> {
        Arc::new(Self {
            db,
            endpoint_id,
            off: AtomicBool::new(false),
        })
    }

    /// Its node's stored id.
    pub(crate) const fn endpoint_id(&self) -> i64 {
        self.endpoint_id
    }

    /// Whether it was turned off since it was loaded.
    pub(crate) fn is_off(&self) -> bool {
        self.off.load(Ordering::Relaxed)
    }

    /// Turns it off for `failure`: at once, and `failure` stored the first
    /// time only; logged as `endpoint.disabled`, warn. A failed write is
    /// logged; it stays off until the next load.
    pub(crate) async fn turn_off(&self, failure: EndpointFailure) {
        if self.off.swap(true, Ordering::Relaxed) {
            return;
        }
        let (node, kind, reason) = (
            self.endpoint_id,
            failure.kind.clone(),
            failure.message.clone(),
        );
        let stored = self
            .db
            .write(move |connection| Ok(endpoint_failure::db::insert(connection, &failure)?))
            .await;
        let Err(error) = stored else {
            tracing::warn!(target: "endpoint.disabled", node, kind, reason, "RPC node disabled");
            return;
        };
        let error = format!("{error:#}");
        tracing::error!(target: "endpoint.disabled", node, error, "RPC node disable not stored");
    }
}
