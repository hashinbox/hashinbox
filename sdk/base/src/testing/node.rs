//! [`Node`]: one in-process node of a mock [`Chain`], with its own view
//! (lag, fork, prune floor, duplicate policy) and [`Faults`].

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rpc::{
    Batch, BoxFuture, CallError, CallResult, Client, Error, Request,
    testing::{Faults, Switch},
};
use serde_json::Value;

use super::{Block, Chain, Model, Submitted, chain::Inner};

/// How a node answers a transaction it already holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Duplicate {
    /// As accepted, again.
    Accept,
    /// With the chain's recorded "already known" refusal.
    #[default]
    Reject,
}

/// Which branch a node serves.
#[derive(Clone, Copy, Debug, Default)]
enum Pin {
    #[default]
    Follow,
    /// Pin to the canonical tip at the next call.
    Next,
    At(usize),
}

#[derive(Default)]
struct Settings {
    lag: u64,
    pin: Pin,
    pruned_below: u64,
    duplicate: Duplicate,
    finality: Option<u64>,
}

/// The controls of one node; clones share them.
#[derive(Clone)]
pub struct NodeControl {
    settings: Arc<Mutex<Settings>>,
    faults: Switch,
}

impl NodeControl {
    /// Its head trails the tip of its branch by `blocks`.
    pub fn lag(&self, blocks: u64) {
        self.settings().lag = blocks;
    }

    /// While `on`, it stays on the branch it serves now, whatever the chain
    /// does; off, it follows the canonical tip again.
    pub fn fork(&self, on: bool) {
        self.settings().pin = if on { Pin::Next } else { Pin::Follow };
    }

    /// It no longer serves what is below `below` (the chain's model says
    /// how: an error, or `null`).
    pub fn prune(&self, below: u64) {
        self.settings().pruned_below = below;
    }

    /// How it answers a transaction it holds.
    pub fn duplicate(&self, policy: Duplicate) {
        self.settings().duplicate = policy;
    }

    /// Its `finalized` trails its head by `blocks`; `None`: as the chain's.
    pub fn finality(&self, blocks: Option<u64>) {
        self.settings().finality = blocks;
    }

    /// Its faults: down, slow, rate limited, failing the next call.
    #[must_use]
    pub const fn faults(&self) -> &Switch {
        &self.faults
    }

    fn settings(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A node's view of the tree while it answers one call.
pub struct View<'a, M: Model> {
    model: &'a M,
    inner: &'a mut Inner<M>,
    branch: Vec<usize>,
    pruned_below: u64,
    duplicate: Duplicate,
    finality: u64,
}

impl<M: Model> View<'_, M> {
    /// The height of its head.
    #[must_use]
    pub fn head(&self) -> u64 {
        self.branch.len() as u64 - 1
    }

    /// Its finalized height: its head less its finality lag (the chain's
    /// unless set); `None` at genesis.
    #[must_use]
    pub fn finalized(&self) -> Option<u64> {
        let head = self.head();
        (head > 0).then(|| head.saturating_sub(self.finality))
    }

    /// Its block at `height`, if it has one.
    #[must_use]
    pub fn block(&self, height: u64) -> Option<&Block<M>> {
        let at = self.branch.get(usize::try_from(height).ok()?)?;
        self.inner.blocks.get(*at)
    }

    /// Its blocks, genesis first.
    pub fn blocks(&self) -> impl Iterator<Item = &Block<M>> {
        self.branch.iter().map(|&at| &self.inner.blocks[at])
    }

    /// Whether it still serves what `height` holds.
    #[must_use]
    pub const fn served(&self, height: u64) -> bool {
        height >= self.pruned_below
    }

    /// How it answers a transaction it holds.
    #[must_use]
    pub const fn duplicate(&self) -> Duplicate {
        self.duplicate
    }

    /// The pending transactions.
    #[must_use]
    pub fn mempool(&self) -> &[M::Tx] {
        &self.inner.mempool
    }

    /// The state at its head with what of the mempool applies.
    #[must_use]
    pub fn pending(&self) -> M::State {
        let mut state = self.head_state();
        for tx in &self.inner.mempool {
            let _ = self.model.apply(&mut state, tx);
        }
        state
    }

    /// The state at its head.
    #[must_use]
    pub fn head_state(&self) -> M::State {
        let at = self.branch[self.branch.len() - 1];
        self.inner.blocks[at].state.clone()
    }

    /// Whether it holds `id`: pending, or in one of its blocks.
    #[must_use]
    pub fn known(&self, id: &str) -> bool {
        self.inner.known(self.model, id, &self.branch)
    }

    /// Adds `tx` to the one mempool unless it is known.
    pub fn submit(&mut self, tx: M::Tx) -> Submitted {
        if self.known(&self.model.id(&tx)) {
            return Submitted::Known;
        }
        self.inner.submit(self.model, tx)
    }
}

/// Answers from the chain, as the settings say.
struct Answering<M: Model> {
    chain: Chain<M>,
    settings: Arc<Mutex<Settings>>,
}

impl<M: Model> Answering<M> {
    fn answer(&self, method: &str, params: &Value) -> CallResult {
        let mut settings = self.settings.lock().unwrap_or_else(PoisonError::into_inner);
        self.chain.with(|model, inner| {
            let canonical = *inner.canonical.last().unwrap_or(&0);
            let tip = match settings.pin {
                Pin::Follow => canonical,
                Pin::Next => {
                    settings.pin = Pin::At(canonical);
                    canonical
                }
                Pin::At(at) => at,
            };
            let mut branch = inner.branch(tip);
            let lag = usize::try_from(settings.lag).unwrap_or(usize::MAX);
            branch.truncate(branch.len().saturating_sub(lag).max(1));
            let finality = settings.finality.unwrap_or(inner.finality);
            let mut view = View {
                model,
                inner,
                branch,
                pruned_below: settings.pruned_below,
                duplicate: settings.duplicate,
                finality,
            };
            model.answer(&mut view, method, params)
        })
    }
}

impl<M: Model> Client for Answering<M> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        let call = request.into_call();
        let answer = self.answer(&call.method, &call.params);
        Box::pin(async move { Ok(answer) })
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        let answers = batch
            .into_calls()
            .iter()
            .map(|call| self.answer(&call.method, &call.params))
            .collect();
        Box::pin(async move { Ok(answers) })
    }
}

/// One node of a mock chain: an [`rpc::Client`] answering from its own
/// view, through its [`Faults`].
pub struct Node<M: Model> {
    faults: Faults<Answering<M>>,
    control: NodeControl,
}

impl<M: Model> Node<M> {
    pub(super) fn new(chain: Chain<M>) -> Self {
        let settings = Arc::new(Mutex::new(Settings::default()));
        let faults = Faults::new(Answering {
            chain,
            settings: settings.clone(),
        });
        let control = NodeControl {
            settings,
            faults: faults.switch(),
        };
        Self { faults, control }
    }

    /// Its controls.
    #[must_use]
    pub fn control(&self) -> NodeControl {
        self.control.clone()
    }
}

impl<M: Model> Client for Node<M> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        self.faults.request(request)
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        self.faults.batch(batch)
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>> {
        self.faults.broadcast(request)
    }
}
