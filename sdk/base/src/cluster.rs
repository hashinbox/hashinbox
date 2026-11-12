//! [`Cluster`]: nodes of one network acting as one [`rpc::Client`], the
//! freshest by [`InfoClient::tip`] serving, as its [`Policy`] selects.

use std::{
    fmt,
    future::poll_fn,
    num::NonZeroU64,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use anyhow::ensure;
use rpc::{Answer, Batch, BoxFuture, Error, RawJson, Request};

use crate::{Future, InfoClient, NodeInfo};

/// No node is waiting to be demoted.
const NONE: usize = usize::MAX;

/// The tunables of a [`Cluster`]'s primary selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// How far, in the tip's unit (blocks, slots), a node may trail the
    /// highest tip and still take over from a failing primary.
    pub max_lag: u64,
    /// Consecutive failed calls on the primary, the latest a transport
    /// failure, that trigger reconsideration.
    pub failure_threshold: NonZeroU64,
    /// Interval after which the next call asks every node its tip again.
    pub refresh: Duration,
    /// What one node's tip may take: a node slower than this is not one to
    /// pick.
    pub probe_timeout: Duration,
}

impl Default for Policy {
    /// Three units of lag, three consecutive failures, a two-minute refresh
    /// and a three-second probe.
    fn default() -> Self {
        Self {
            max_lag: 3,
            failure_threshold: NonZeroU64::new(3).expect("three is nonzero"),
            refresh: Duration::from_secs(120),
            probe_timeout: Duration::from_secs(3),
        }
    }
}

/// Nodes of one network acting as one [`rpc::Client`].
///
/// Requests and batches go to the primary. [`rpc::Client::broadcast`]
/// reaches every node exactly once, concurrently, and waits for all of them,
/// so no node's execution is cancelled by an earlier answer.
///
/// The primary is chosen by every node's [`InfoClient::tip`] at the first call and
/// whenever a call arrives after a trigger: a transport failure brought the
/// primary to [`Policy::failure_threshold`] consecutive failed calls, it was
/// rate-limited ([`Error::is_rate_limited`]), or [`Policy::refresh`]
/// elapsed. The highest tip wins and a tie keeps the incumbent. A triggered
/// primary yields to the highest other node within [`Policy::max_lag`] of
/// the highest tip, and stays if there is none; it remains a candidate for
/// every later selection. A tip that fails or takes longer than
/// [`Policy::probe_timeout`] only makes that node ineligible. Probing runs
/// inside a call, never in a task of its own, and one call probes at a time.
///
/// It is an [`InfoClient`] too, the primary's, so clusters nest. The primary is
/// identified by its index in the order given to [`Self::new`]; `Debug`
/// shows the node count and that index only.
pub struct Cluster {
    nodes: Vec<Arc<dyn InfoClient>>,
    policy: Policy,
    primary: AtomicUsize,
    /// The primary's consecutive failed calls, as forwarded.
    failures: AtomicU64,
    /// The primary that hit a trigger, until a selection replaces it.
    demoted: AtomicUsize,
    probing: AtomicBool,
    /// When nodes were last probed; `None` until the first call.
    refreshed: Mutex<Option<Instant>>,
}

impl fmt::Debug for Cluster {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cluster")
            .field("node_count", &self.nodes.len())
            .field("primary", &self.primary())
            .finish()
    }
}

impl Cluster {
    /// The first node primary, asking no node anything: the first call
    /// probes every node and makes the highest tip primary; a tie, or no
    /// answering node, leaves the first node primary.
    ///
    /// # Errors
    ///
    /// When `nodes` is empty.
    pub fn new(nodes: Vec<Arc<dyn InfoClient>>, policy: Policy) -> anyhow::Result<Self> {
        ensure!(!nodes.is_empty(), "a cluster needs at least one node");
        Ok(Self {
            nodes,
            policy,
            primary: AtomicUsize::new(0),
            failures: AtomicU64::new(0),
            demoted: AtomicUsize::new(NONE),
            probing: AtomicBool::new(false),
            refreshed: Mutex::new(None),
        })
    }

    /// The current primary's index in the order given to [`Self::new`]: an
    /// opaque node id that carries no endpoint material.
    #[must_use]
    pub fn primary(&self) -> usize {
        self.primary.load(Ordering::Acquire)
    }

    async fn unicast(&self, request: Request) -> Result<RawJson, Error> {
        let (index, node) = self.current().await;
        let outcome = node.request(request).await;
        self.observe(index, outcome.is_ok(), outcome.as_ref().err());
        outcome
    }

    async fn unicast_batch(&self, batch: Batch) -> Result<Vec<Answer>, Error> {
        let (index, node) = self.current().await;
        let outcome = node.batch(batch).await;
        let answered = matches!(&outcome, Ok(results) if results.iter().all(Result::is_ok));
        self.observe(index, answered, outcome.as_ref().err());
        outcome
    }

    /// Every node once; the first success answers. Otherwise a transport
    /// failure wins over an error object, because a node whose outcome is
    /// unknown may have accepted what the others refused.
    async fn fan_out(&self, request: Request) -> Result<(), Error> {
        let sends = self
            .nodes
            .iter()
            .map(|node| node.broadcast(request.clone()))
            .collect();
        let mut failed = None;
        for outcome in settle(sends).await {
            match outcome {
                Ok(()) => return Ok(()),
                Err(error @ Error::Transport { .. }) => failed = Some(error),
                Err(refusal) => failed = failed.or(Some(refusal)),
            }
        }
        Err(failed.expect("a cluster holds at least one node"))
    }

    /// The primary after any due selection.
    async fn current(&self) -> (usize, &dyn InfoClient) {
        let demoted = self.demoted.load(Ordering::Acquire);
        if demoted != NONE || self.refresh_due() {
            self.select(demoted).await;
        }
        let index = self.primary();
        (index, &*self.nodes[index])
    }

    /// Counts the primary's outcome, and marks it for demotion when that is
    /// a trigger: a rate limit, or a transport failure completing the
    /// failure threshold. An error object is a node answering, such as "not
    /// found" or a revert, so it extends a streak but never completes one.
    fn observe(&self, index: usize, answered: bool, failed: Option<&Error>) {
        if index != self.primary() {
            return;
        }
        if answered {
            self.failures.store(0, Ordering::Release);
            return;
        }
        let failures = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
        let Some(error) = failed else {
            return;
        };
        let completes = matches!(error, Error::Transport { .. })
            && failures >= self.policy.failure_threshold.get();
        if error.is_rate_limited() || completes {
            self.demoted.store(index, Ordering::Release);
        }
    }

    /// Probes every node and applies the choice; `demoted` is excluded while
    /// it is still primary. A selection already running elsewhere suffices.
    async fn select(&self, demoted: usize) {
        if self.probing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _claim = Claim(&self.probing);
        let probes = self.nodes.iter().map(|node| self.probe(&**node)).collect();
        let tips = settle(probes).await;
        let incumbent = self.primary();
        let excluded = (demoted == incumbent).then_some(incumbent);
        let chosen = choose(&tips, incumbent, excluded, self.policy.max_lag);
        if chosen != incumbent {
            self.failures.store(0, Ordering::Release);
        }
        self.primary.store(chosen, Ordering::Release);
        let _ = self
            .demoted
            .compare_exchange(demoted, NONE, Ordering::AcqRel, Ordering::Acquire);
        *self
            .refreshed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    /// `node`'s tip; `None` when it fails or is too slow.
    fn probe<'a>(&self, node: &'a dyn InfoClient) -> BoxFuture<'a, Option<u64>> {
        let timeout = self.policy.probe_timeout;
        Box::pin(async move { tokio::time::timeout(timeout, node.tip()).await.ok()?.ok() })
    }

    fn refresh_due(&self) -> bool {
        let refreshed = *self
            .refreshed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        refreshed.is_none_or(|at| at.elapsed() >= self.policy.refresh)
    }
}

impl rpc::Client for Cluster {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(self.unicast(request))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.unicast_batch(batch))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.fan_out(request))
    }
}

/// The primary's.
impl InfoClient for Cluster {
    fn tip(&self) -> Future<'_, anyhow::Result<u64>> {
        Box::pin(async move { self.current().await.1.tip().await })
    }

    fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(async move { self.current().await.1.info().await })
    }
}

/// Releases the probing claim, also when a selection is cancelled.
struct Claim<'a>(&'a AtomicBool);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The highest of `tips` (by node) among eligible nodes, a tie keeping the
/// incumbent and otherwise the lowest index. `excluded` is eligible for
/// nothing; the others must be within `max_lag` of the highest tip of all.
/// No eligible node keeps the incumbent.
fn choose(tips: &[Option<u64>], incumbent: usize, excluded: Option<usize>, max_lag: u64) -> usize {
    let Some(highest) = tips.iter().flatten().max() else {
        return incumbent;
    };
    let floor = highest.saturating_sub(max_lag);
    tips.iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != excluded)
        .filter_map(|(index, tip)| tip.filter(|tip| *tip >= floor).map(|tip| (index, tip)))
        .max_by_key(|(index, tip)| (*tip, *index == incumbent, std::cmp::Reverse(*index)))
        .map_or(incumbent, |(index, _)| index)
}

/// Drives every future to completion concurrently; outputs in input order.
async fn settle<T>(futures: Vec<BoxFuture<'_, T>>) -> Vec<T> {
    let mut pending: Vec<_> = futures.into_iter().map(Some).collect();
    let mut settled: Vec<_> = pending.iter().map(|_| None).collect();
    poll_fn(|context| {
        for (slot, output) in pending.iter_mut().zip(&mut settled) {
            let Some(future) = slot else {
                continue;
            };
            if let Poll::Ready(ready) = future.as_mut().poll(context) {
                *output = Some(ready);
                *slot = None;
            }
        }
        if pending.iter().any(Option::is_some) {
            return Poll::Pending;
        }
        Poll::Ready(settled.drain(..).flatten().collect())
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rpc::{
        Answer, Batch, BoxFuture, Client, Counted, Error, RawJson, Request,
        testing::{Fault, Faults, Reply, Script, Switch},
    };
    use serde_json::json;

    use super::{Cluster, Policy};
    use crate::{Future, InfoClient, NodeInfo};

    /// A scripted node whose tip is its `tip` call.
    struct Node(Counted<Faults<Script>>);

    impl Client for Node {
        fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
            self.0.request(request)
        }

        fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
            self.0.batch(batch)
        }

        fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
            self.0.broadcast(request)
        }
    }

    impl InfoClient for Node {
        fn tip(&self) -> Future<'_, anyhow::Result<u64>> {
            Box::pin(async move {
                let raw = self.request(Request::new("tip", json!([]))).await?;
                Ok(raw.deserialize()?)
            })
        }

        fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>> {
            Box::pin(async { anyhow::bail!("not asked") })
        }
    }

    fn node(replies: Vec<Reply>) -> (Arc<Node>, Script, Switch) {
        let script = Script::new(replies);
        let faults = Faults::new(script.clone());
        let switch = faults.switch();
        (Arc::new(Node(Counted::new(faults))), script, switch)
    }

    async fn ask(cluster: &Cluster) -> Result<u64, Error> {
        let raw = cluster.request(Request::new("m", json!([]))).await?;
        raw.deserialize()
    }

    #[tokio::test]
    async fn the_highest_tip_is_primary_and_a_rate_limit_demotes_it() {
        let (a, sa, _) = node(vec![
            Reply::ok("tip", json!(5)),
            Reply::ok("tip", json!(9)),
            Reply::ok("m", json!(0)),
        ]);
        let (b, sb, wb) = node(vec![
            Reply::ok("tip", json!(9)),
            Reply::ok("m", json!(1)),
            Reply::ok("tip", json!(9)),
        ]);
        let cluster = Cluster::new(vec![a, b.clone()], Policy::default()).unwrap();
        assert_eq!(ask(&cluster).await.unwrap(), 1);
        assert_eq!(cluster.primary(), 1);

        let limit = "Rate limit exceeded".to_owned();
        wb.fail_next(Some("m"), Fault::Remote(-32_005, limit));
        ask(&cluster).await.unwrap_err();
        assert_eq!(
            ask(&cluster).await.unwrap(),
            0,
            "a tie, the limited node excluded"
        );
        assert_eq!(cluster.primary(), 0);
        let counted = b.0.statistics().snapshot();
        assert_eq!((counted.attempts, counted.failures), (4, 1), "probes count");
        sa.exhausted();
        sb.exhausted();
    }

    #[tokio::test]
    async fn a_broadcast_reaches_every_node_once() {
        let (a, sa, _) = node(vec![Reply::ok("send", json!("id"))]);
        let (b, sb, _) = node(vec![Reply::ok("send", json!("id"))]);
        let cluster = Cluster::new(vec![a.clone(), b.clone()], Policy::default()).unwrap();
        cluster
            .broadcast(Request::once("send", json!([])))
            .await
            .unwrap();
        for node in [a, b] {
            assert_eq!(node.0.statistics().snapshot().attempts, 1);
        }
        sa.exhausted();
        sb.exhausted();
    }
}
