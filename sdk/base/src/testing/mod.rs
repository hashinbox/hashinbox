//! Chain-neutral test doubles: one mock chain ([`Chain`]: block tree,
//! mempool, logical clock) seen by several in-process nodes ([`Node`]: each
//! its own view and faults, each an [`rpc::Client`]). A chain supplies its
//! [`Model`] (state, transactions, answers rendered from recorded node
//! replies) and its [`TestChain`]; [`Recording`] keeps those answers honest.
//!
//! Behind the `testing` feature: dev-dependencies only, offline, no sockets.

mod chain;
mod node;
mod shapes;

use std::sync::Arc;

pub use chain::{Block, Chain, Clock, Model, Submitted};
pub use node::{Duplicate, Node, NodeControl, View};
pub use shapes::Recording;

use crate::NetworkConfig;

/// What a [`TestChain`] can show; a case needing more is skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps(u32);

impl Caps {
    /// Nothing beyond the native currency on one node.
    pub const NONE: Self = Self(0);
    /// A token besides the native currency.
    pub const TOKEN: Self = Self(1);
    /// Replacing a pending transaction.
    pub const REPLACE: Self = Self(1 << 1);
    /// Taking blocks back.
    pub const REORG: Self = Self(1 << 2);
    /// Nodes that stop serving old history.
    pub const PRUNE: Self = Self(1 << 3);
    /// Skipped positions.
    pub const SKIP: Self = Self(1 << 4);
    /// Several nodes of one network.
    pub const CLUSTER: Self = Self(1 << 5);

    /// Both sets.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every capability of `other` is in this set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// How the server reaches a test network's nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topology {
    /// One node.
    Single,
    /// Two nodes behind the chain's cluster.
    Cluster,
}

impl Topology {
    /// How many nodes it runs.
    #[must_use]
    pub const fn nodes(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Cluster => 2,
        }
    }
}

/// One chain's mock network as the server suite drives it: its nodes, its
/// configuration, and the controls cases move it with. Amounts are atomic
/// units; assets are `native` or a token's id.
pub trait TestChain: Sized + Send + Sync + 'static {
    /// What it can show.
    const CAPS: Caps;
    /// The method a node takes a signed transaction by.
    const SUBMIT: &'static str;
    /// A chain with `nodes` nodes, past genesis and with a final block.
    fn start(nodes: usize) -> Self;
    /// Its nodes, in order.
    fn nodes(&self) -> Vec<Arc<dyn rpc::Client>>;
    /// The chain's own tip probe, which its cluster picks a primary by.
    fn height(node: &Arc<dyn rpc::Client>) -> rpc::BoxFuture<'_, Result<u64, rpc::CallError>>;
    /// The network as the server stores it; endpoints are left to the caller.
    fn network(&self) -> NetworkConfig;
    /// Pays `amount` of `asset` to `to`, mined by the next [`Self::mine`].
    fn fund(&self, to: &str, amount: &str, asset: &str);
    /// The canonical tip's height.
    fn tip(&self) -> u64;
    /// Mines `n` blocks.
    fn mine(&self, n: u64);
    /// While `on`, mined blocks leave the mempool alone.
    fn hold(&self, on: bool);
    /// Takes the last `depth` blocks back (their transactions return to
    /// the mempool) and mines a longer branch without them.
    fn reorg(&self, depth: u64);
    /// Every node stops serving what is below `below`.
    fn prune(&self, below: u64);
    /// The controls of node `index`.
    fn node(&self, index: usize) -> NodeControl;
}
