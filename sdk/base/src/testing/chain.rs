//! [`Chain`]: one mock chain — a block tree with its canonical branch, one
//! mempool, a logical clock — and the [`Model`] a chain fills it with.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rpc::CallResult;
use serde_json::Value;

use super::{Node, View};

/// One chain's rules: its state, its transactions, and how its nodes answer.
pub trait Model: Send + Sync + Sized + 'static {
    /// What every account (or coin) holds after a block.
    type State: Clone + Send + Sync;
    /// One transaction.
    type Tx: Clone + Send + Sync;
    /// What including a transaction did: its receipt.
    type Outcome: Clone + Send + Sync;
    /// A block's own identity: its hash and its parent's.
    type Seal: Clone + Send + Sync;

    /// The transaction's id.
    fn id(&self, tx: &Self::Tx) -> String;

    /// Applies `tx` to `state` as a block includes it; `None`, and `state`
    /// unchanged, when it cannot be included (yet).
    fn apply(&self, state: &mut Self::State, tx: &Self::Tx) -> Option<Self::Outcome>;

    /// The seal of the block at `height` over `parent`, on branch `branch`
    /// (a reorg's replacement blocks are another branch, so another hash).
    fn seal(&self, parent: Option<&Self::Seal>, height: u64, branch: u64) -> Self::Seal;

    /// A node's answer to `method` with `params`, over its `view`; an
    /// unknown method gets the chain's recorded method-not-found error.
    fn answer(&self, view: &mut View<'_, Self>, method: &str, params: &Value) -> CallResult;
}

/// One block of the tree.
pub struct Block<M: Model> {
    /// Its height.
    pub height: u64,
    /// Its logical time, in seconds.
    pub time: u64,
    /// Its identity.
    pub seal: M::Seal,
    /// What it includes, in order, and what each did.
    pub txs: Vec<(M::Tx, M::Outcome)>,
    /// The state after it.
    pub state: M::State,
    pub(super) parent: Option<usize>,
}

/// Logical time: each block adds one block time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    now: u64,
    step: u64,
}

impl Clock {
    /// Starts at `now`, in seconds; every block adds `step`.
    #[must_use]
    pub const fn new(now: u64, step: u64) -> Self {
        Self { now, step }
    }

    fn tick(&mut self) -> u64 {
        self.now += self.step;
        self.now
    }
}

/// What a submission found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// New: now in the mempool.
    New,
    /// Already in the mempool or a canonical block.
    Known,
}

/// The tree and everything that moves it.
pub(super) struct Inner<M: Model> {
    pub(super) blocks: Vec<Block<M>>,
    pub(super) canonical: Vec<usize>,
    pub(super) mempool: Vec<M::Tx>,
    clock: Clock,
    held: bool,
    /// Blocks a node's `finalized` trails its head by.
    pub(super) finality: u64,
    branches: u64,
}

impl<M: Model> Inner<M> {
    fn mine(&mut self, model: &M, include: bool) {
        let Some(&at) = self.canonical.last() else {
            return;
        };
        let parent = &self.blocks[at];
        let mut state = parent.state.clone();
        let height = parent.height + 1;
        let seal = model.seal(Some(&parent.seal), height, self.branches);
        let mut txs = Vec::new();
        if include {
            let mut kept = Vec::new();
            for tx in std::mem::take(&mut self.mempool) {
                match model.apply(&mut state, &tx) {
                    Some(outcome) => txs.push((tx, outcome)),
                    None => kept.push(tx),
                }
            }
            self.mempool = kept;
        }
        self.blocks.push(Block {
            height,
            time: self.clock.tick(),
            seal,
            txs,
            state,
            parent: Some(at),
        });
        self.canonical.push(self.blocks.len() - 1);
    }

    /// Whether `id` is in the mempool or on the branch of `tip`.
    pub(super) fn known(&self, model: &M, id: &str, branch: &[usize]) -> bool {
        self.mempool.iter().any(|tx| model.id(tx) == id)
            || branch
                .iter()
                .any(|&at| self.blocks[at].txs.iter().any(|(tx, _)| model.id(tx) == id))
    }

    pub(super) fn submit(&mut self, model: &M, tx: M::Tx) -> Submitted {
        let canonical = self.canonical.clone();
        if self.known(model, &model.id(&tx), &canonical) {
            return Submitted::Known;
        }
        self.mempool.push(tx);
        Submitted::New
    }

    /// The block indices from genesis through `tip`.
    pub(super) fn branch(&self, tip: usize) -> Vec<usize> {
        let mut branch = vec![tip];
        while let Some(parent) = self.blocks[*branch.last().unwrap_or(&0)].parent {
            branch.push(parent);
        }
        branch.reverse();
        branch
    }
}

/// One mock chain; clones share it. Its nodes ([`Self::node`]) each see it
/// through their own view.
pub struct Chain<M: Model> {
    model: Arc<M>,
    inner: Arc<Mutex<Inner<M>>>,
}

impl<M: Model> Clone for Chain<M> {
    fn clone(&self) -> Self {
        Self {
            model: self.model.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<M: Model> Chain<M> {
    /// Genesis (block 0) holding `genesis`, at the clock's start.
    #[must_use]
    pub fn new(model: M, genesis: M::State, clock: Clock) -> Self {
        let seal = model.seal(None, 0, 0);
        let genesis = Block {
            height: 0,
            time: clock.now,
            seal,
            txs: Vec::new(),
            state: genesis,
            parent: None,
        };
        let inner = Inner {
            blocks: vec![genesis],
            canonical: vec![0],
            mempool: Vec::new(),
            clock,
            held: false,
            finality: 0,
            branches: 0,
        };
        Self {
            model: Arc::new(model),
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Its rules.
    #[must_use]
    pub fn model(&self) -> &M {
        &self.model
    }

    /// The canonical tip's height.
    #[must_use]
    pub fn tip(&self) -> u64 {
        self.lock().canonical.len() as u64 - 1
    }

    /// Mines `n` blocks on the canonical tip, each including what of the
    /// mempool applies (nothing while held).
    pub fn mine(&self, n: u64) {
        let mut inner = self.lock();
        for _ in 0..n {
            let include = !inner.held;
            inner.mine(&self.model, include);
        }
    }

    /// While `on`, mined blocks leave the mempool alone.
    pub fn hold(&self, on: bool) {
        self.lock().held = on;
    }

    /// Takes the last `depth` canonical blocks back (kept as an old branch;
    /// their transactions return to the front of the mempool) and mines
    /// `depth + 1` empty blocks on another branch.
    pub fn reorg(&self, depth: u64) {
        let mut inner = self.lock();
        let depth = usize::try_from(depth)
            .unwrap_or(usize::MAX)
            .min(inner.canonical.len() - 1);
        let keep = inner.canonical.len() - depth;
        let taken = inner.canonical.split_off(keep);
        let mut back: Vec<M::Tx> = taken
            .iter()
            .flat_map(|&at| inner.blocks[at].txs.iter().map(|(tx, _)| tx.clone()))
            .collect();
        back.append(&mut inner.mempool);
        inner.mempool = back;
        inner.branches += 1;
        for _ in 0..=depth {
            inner.mine(&self.model, false);
        }
    }

    /// Nodes report `finalized` this many blocks below their head.
    pub fn finality_lag(&self, blocks: u64) {
        self.lock().finality = blocks;
    }

    /// Adds `tx` to the mempool unless it is known.
    pub fn submit(&self, tx: M::Tx) -> Submitted {
        self.lock().submit(&self.model, tx)
    }

    /// The canonical tip's state with what of the mempool applies.
    #[must_use]
    pub fn pending(&self) -> M::State {
        let inner = self.lock();
        let at = *inner.canonical.last().unwrap_or(&0);
        let mut state = inner.blocks[at].state.clone();
        for tx in &inner.mempool {
            let _ = self.model.apply(&mut state, tx);
        }
        state
    }

    /// A new node of this chain, following the canonical tip.
    #[must_use]
    pub fn node(&self) -> Node<M> {
        Node::new(self.clone())
    }

    /// Runs `answer` over a node's view of the tree.
    pub(super) fn with<T>(&self, view: impl FnOnce(&M, &mut Inner<M>) -> T) -> T {
        view(&self.model, &mut self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, Inner<M>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use rpc::{Client as _, RawJson, Request};
    use serde_json::json;

    use super::*;
    use crate::testing::Duplicate;

    /// Transactions are numbers, the state their sum; a block's seal is
    /// its branch and height.
    struct Sum;

    impl Model for Sum {
        type State = u64;
        type Tx = u64;
        type Outcome = ();
        type Seal = (u64, u64);

        fn id(&self, tx: &u64) -> String {
            tx.to_string()
        }

        fn apply(&self, state: &mut u64, tx: &u64) -> Option<()> {
            *state += tx;
            Some(())
        }

        fn seal(&self, _parent: Option<&(u64, u64)>, height: u64, branch: u64) -> (u64, u64) {
            (branch, height)
        }

        fn answer(&self, view: &mut View<'_, Self>, method: &str, params: &Value) -> CallResult {
            let answer = match method {
                "head" => json!([view.head(), view.finalized(), view.head_state()]),
                "send" => json!(view.submit(params[0].as_u64().unwrap_or(0)) == Submitted::New),
                _ => json!(view.served(params[0].as_u64().unwrap_or(0))),
            };
            Ok(RawJson::from_serializable(&answer).unwrap())
        }
    }

    fn ask(node: &Node<Sum>, method: &str, params: Value) -> Value {
        let reply = futures_executor::block_on(node.request(Request::new(method, params)));
        reply.unwrap().unwrap().deserialize().unwrap()
    }

    #[test]
    fn nodes_see_the_chain_through_their_own_view() {
        let chain = Chain::new(Sum, 0, Clock::new(0, 12));
        chain.finality_lag(2);
        let (node, behind) = (chain.node(), chain.node());
        behind.control().lag(2);
        assert_eq!(ask(&node, "send", json!([5])), json!(true));
        assert_eq!(ask(&behind, "send", json!([5])), json!(false));
        chain.mine(4);
        assert_eq!(ask(&node, "head", json!([])), json!([4, 2, 5]));
        assert_eq!(ask(&behind, "head", json!([])), json!([2, 0, 5]));
        node.control().prune(3);
        assert_eq!(ask(&node, "served", json!([2])), json!(false));
        node.control().duplicate(Duplicate::Accept);
        node.control().finality(Some(0));
        assert_eq!(ask(&node, "head", json!([])), json!([4, 4, 5]));
    }

    #[test]
    fn a_reorg_returns_transactions_and_a_forked_node_stays() {
        let chain = Chain::new(Sum, 0, Clock::new(0, 12));
        let (node, forked) = (chain.node(), chain.node());
        chain.mine(2);
        chain.submit(7);
        chain.mine(1);
        forked.control().fork(true);
        assert_eq!(ask(&forked, "head", json!([])), json!([3, 3, 7]));
        chain.reorg(1);
        assert_eq!(ask(&node, "head", json!([])), json!([4, 4, 0]));
        assert_eq!(ask(&forked, "head", json!([])), json!([3, 3, 7]));
        assert_eq!(chain.pending(), 7);
        chain.mine(1);
        assert_eq!(ask(&node, "head", json!([])), json!([5, 5, 7]));
    }
}
