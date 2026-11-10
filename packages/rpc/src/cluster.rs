use std::{
    fmt,
    future::poll_fn,
    num::NonZeroU64,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use crate::{
    Batch, BoxFuture, CallError, CallResult, Client, Error, ErrorKind, Request, Statistics,
    client::{narrow, widen},
};

/// No node is waiting to be demoted.
const NONE: usize = usize::MAX;

/// A failure classification that marks a node as rate-limited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// The endpoint answered with this HTTP status.
    Http(u16),
    /// The node answered with a JSON-RPC error object carrying this code.
    Remote(i64),
}

/// The tunables of a [`Cluster`]'s primary selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// How far, in the probe's unit (blocks, slots), a node may trail the
    /// highest probed tip and still take over from a failing primary.
    pub max_lag: u64,
    /// Consecutive failed calls on the primary, the latest a transport
    /// failure, that trigger reconsideration.
    pub failure_threshold: NonZeroU64,
    /// Classifications that trigger reconsideration on their first occurrence.
    pub rate_limit_codes: Vec<Limit>,
    /// Interval after which the next call re-probes every node.
    pub refresh: Duration,
}

impl Default for Policy {
    /// Three units of lag, three consecutive failures, HTTP 429 and the
    /// JSON-RPC "limit exceeded" code `-32005`, and a two-minute refresh.
    fn default() -> Self {
        Self {
            max_lag: 3,
            failure_threshold: NonZeroU64::new(3).expect("three is nonzero"),
            rate_limit_codes: vec![Limit::Http(429), Limit::Remote(-32_005)],
            refresh: Duration::from_secs(120),
        }
    }
}

/// Several nodes of one network acting as one [`Client`].
///
/// Requests and batches go to the current primary. [`Client::broadcast`]
/// reaches every node exactly once, concurrently, and waits for all of them,
/// so no node's execution is cancelled by an earlier answer.
///
/// The primary is chosen by an injected tip probe — the chain's own height
/// call — at the first call and whenever a call arrives after a
/// trigger: a transport failure brought the primary to
/// [`Policy::failure_threshold`] consecutive failures, its latest failure is
/// one of [`Policy::rate_limit_codes`], or [`Policy::refresh`] elapsed. The
/// highest tip wins and a tie keeps the incumbent. A triggered primary yields
/// to the highest other node within [`Policy::max_lag`] of the highest tip,
/// and stays if there is none; it remains a candidate for every later
/// selection. Probing runs inside a call, never in a task of its own, and one
/// call probes at a time.
///
/// The primary is identified by its index in the order given to
/// [`Self::new`]. `Debug` shows the node count and that index only.
pub struct Cluster<C> {
    first: Node<C>,
    rest: Vec<Node<C>>,
    selection: Option<Selection<C>>,
}

struct Node<C> {
    client: C,
    statistics: Statistics,
}

/// The tip probe, boxed so the cluster's type names only its clients.
type Probe<C> = Box<dyn for<'a> Fn(&'a C) -> BoxFuture<'a, Result<u64, CallError>> + Send + Sync>;

/// Primary selection state; absent for a single node, which has no choice.
struct Selection<C> {
    probe: Probe<C>,
    policy: Policy,
    primary: AtomicUsize,
    /// The primary that hit a trigger, until a selection replaces it.
    demoted: AtomicUsize,
    probing: AtomicBool,
    /// When nodes were last probed; `None` until the first call.
    refreshed: Mutex<Option<Instant>>,
}

impl<C> fmt::Debug for Cluster<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cluster")
            .field("node_count", &(self.rest.len() + 1))
            .field("primary", &self.primary())
            .finish()
    }
}

impl<C: Client> Cluster<C> {
    /// The first node primary, asking no node anything: the first call
    /// probes every node and makes the highest tip primary; a tie, or no
    /// answering probe, leaves the first node primary.
    ///
    /// `probe` reads one node's tip in the chain's own unit. A probe failure
    /// only makes that node ineligible for this selection.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when `clients` is empty.
    pub fn new<P>(clients: Vec<C>, probe: P, policy: Policy) -> Result<Self, Error>
    where
        P: for<'a> Fn(&'a C) -> BoxFuture<'a, Result<u64, CallError>> + Send + Sync + 'static,
    {
        let mut nodes = clients.into_iter().map(Node::new);
        let Some(first) = nodes.next() else {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "JSON-RPC requires at least one endpoint",
            ));
        };
        Ok(Self {
            first,
            rest: nodes.collect(),
            selection: Some(Selection {
                probe: Box::new(probe),
                policy,
                primary: AtomicUsize::new(0),
                demoted: AtomicUsize::new(NONE),
                probing: AtomicBool::new(false),
                refreshed: Mutex::new(None),
            }),
        })
    }

    /// One node: every call reaches it, and nothing is probed.
    #[must_use]
    pub fn one(client: C) -> Self {
        Self {
            first: Node::new(client),
            rest: Vec::new(),
            selection: None,
        }
    }

    /// Calls the cluster routed to each node, in the order given to
    /// [`Self::new`]. A node's own transport counters, such as
    /// [`crate::Http::statistics`], count its executions instead.
    #[must_use]
    pub fn statistics(&self) -> Vec<Statistics> {
        self.nodes().map(|node| node.statistics.clone()).collect()
    }

    async fn unicast(&self, request: Request) -> Result<CallResult, Error> {
        let (index, node) = self.current().await;
        node.statistics.begin();
        let outcome = node.client.request(request).await;
        node.statistics.call(&outcome);
        self.observe(index, node, outcome.is_err());
        outcome
    }

    async fn unicast_batch(&self, batch: Batch) -> Result<Vec<CallResult>, Error> {
        let (index, node) = self.current().await;
        node.statistics.begin();
        let outcome = node.client.batch(batch).await;
        node.statistics.batch(&outcome);
        self.observe(index, node, outcome.is_err());
        outcome
    }

    /// Every node once; the first success answers. Otherwise a transport
    /// failure wins over an error object, because a node whose outcome is
    /// unknown may have accepted what the others refused.
    async fn fan_out(&self, request: Request) -> Result<CallResult, Error> {
        // Judged as calls: statistics and the winner rule read call outcomes.
        let sends = self
            .nodes()
            .map(|node| {
                let request = request.clone();
                Box::pin(async move {
                    node.statistics.begin();
                    let outcome = widen(node.client.broadcast(request).await);
                    node.statistics.call(&outcome);
                    outcome
                }) as BoxFuture<'_, _>
            })
            .collect();
        let mut answered = None;
        let mut unknown = None;
        for outcome in settle(sends).await {
            match outcome {
                Ok(Ok(result)) => return Ok(Ok(result)),
                Ok(Err(failure)) => answered = Some(failure),
                Err(error) => unknown = Some(error),
            }
        }
        match (unknown, answered) {
            (Some(error), _) => Err(error),
            (None, Some(failure)) => Ok(Err(failure)),
            (None, None) => unreachable!("a cluster holds at least one node"),
        }
    }

    /// The primary after any due selection.
    async fn current(&self) -> (usize, &Node<C>) {
        if let Some(selection) = &self.selection {
            let demoted = selection.demoted.load(Ordering::Acquire);
            if demoted != NONE || selection.refresh_due() {
                self.select(demoted).await;
            }
        }
        let index = self.primary();
        (index, self.node(index))
    }

    /// Marks the primary for demotion when its latest outcome is a trigger.
    /// The failure threshold is checked on a transport failure only: an error
    /// object is a node answering, such as "not found" or a revert, so it
    /// extends a streak but never completes one.
    fn observe(&self, index: usize, node: &Node<C>, transport_failed: bool) {
        let Some(selection) = &self.selection else {
            return;
        };
        let policy = &selection.policy;
        let failures = node.statistics.snapshot().consecutive_failures;
        if (transport_failed && failures >= policy.failure_threshold.get())
            || node.statistics.limited(&policy.rate_limit_codes)
        {
            selection.demoted.store(index, Ordering::Release);
        }
    }

    /// Probes every node and applies the choice; `demoted` is excluded while
    /// it is still primary. A selection already running elsewhere suffices.
    async fn select(&self, demoted: usize) {
        let Some(selection) = &self.selection else {
            return;
        };
        if selection.probing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _claim = Claim(&selection.probing);
        let probes = self
            .nodes()
            .enumerate()
            .map(|(index, node)| {
                Box::pin(async move { (index, (selection.probe)(&node.client).await.ok()) })
                    as BoxFuture<'_, _>
            })
            .collect();
        let tips = Tips::new(settle(probes).await);
        let incumbent = selection.primary.load(Ordering::Acquire);
        let excluded = (demoted == incumbent).then_some(incumbent);
        let chosen = tips.choose(incumbent, excluded, selection.policy.max_lag);
        selection.primary.store(chosen, Ordering::Release);
        let _ =
            selection
                .demoted
                .compare_exchange(demoted, NONE, Ordering::AcqRel, Ordering::Acquire);
        *selection
            .refreshed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }
}

impl<C> Cluster<C> {
    /// The current primary's index in the order given to [`Self::new`]: an
    /// opaque node id that carries no endpoint material.
    #[must_use]
    pub fn primary(&self) -> usize {
        self.selection
            .as_ref()
            .map_or(0, |selection| selection.primary.load(Ordering::Acquire))
    }

    fn nodes(&self) -> impl Iterator<Item = &Node<C>> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    fn node(&self, index: usize) -> &Node<C> {
        index
            .checked_sub(1)
            .and_then(|rest| self.rest.get(rest))
            .unwrap_or(&self.first)
    }
}

impl<C: Client> Client for Cluster<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        Box::pin(self.unicast(request))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        Box::pin(self.unicast_batch(batch))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move { narrow(self.fan_out(request).await) })
    }
}

impl<C> Node<C> {
    fn new(client: C) -> Self {
        Self {
            client,
            statistics: Statistics::default(),
        }
    }
}

impl<C> Selection<C> {
    fn refresh_due(&self) -> bool {
        let refreshed = *self
            .refreshed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        refreshed.is_none_or(|at| at.elapsed() >= self.policy.refresh)
    }
}

/// Releases the probing claim, also when a selection is cancelled.
struct Claim<'a>(&'a AtomicBool);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One probe round: each node's tip by index, `None` where the probe failed.
struct Tips(Vec<Option<u64>>);

impl Tips {
    /// Orders probe outcomes, which settle in completion order, by node.
    fn new(mut probed: Vec<(usize, Option<u64>)>) -> Self {
        probed.sort_by_key(|(index, _)| *index);
        Self(probed.into_iter().map(|(_, tip)| tip).collect())
    }

    /// The highest tip among eligible nodes, a tie keeping the incumbent and
    /// otherwise the lowest index. `excluded` is eligible for nothing; the
    /// others must be within `max_lag` of the highest tip of all. No eligible
    /// node keeps the incumbent.
    fn choose(&self, incumbent: usize, excluded: Option<usize>, max_lag: u64) -> usize {
        let Some(highest) = self.0.iter().flatten().max() else {
            return incumbent;
        };
        let floor = highest.saturating_sub(max_lag);
        self.0
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != excluded)
            .filter_map(|(index, tip)| tip.filter(|tip| *tip >= floor).map(|tip| (index, tip)))
            .max_by_key(|(index, tip)| (*tip, *index == incumbent, std::cmp::Reverse(*index)))
            .map_or(incumbent, |(index, _)| index)
    }
}

/// Drives every future to completion concurrently; outputs in completion order.
async fn settle<'a, T>(futures: Vec<BoxFuture<'a, T>>) -> Vec<T> {
    let mut pending: Vec<_> = futures.into_iter().map(Some).collect();
    let mut settled = Vec::with_capacity(pending.len());
    poll_fn(|context| {
        settled.extend(pending.iter_mut().filter_map(|slot| {
            let Poll::Ready(output) = slot.as_mut()?.as_mut().poll(context) else {
                return None;
            };
            *slot = None;
            Some(output)
        }));
        if pending.iter().all(Option::is_none) {
            Poll::Ready(std::mem::take(&mut settled))
        } else {
            Poll::Pending
        }
    })
    .await
}
