//! [`InfoClient`]: a chain's client over a node, telling its tip and chain;
//! and [`NodeInfo`], what it tells.

use crate::Future;

/// A chain's client over a node: what a [`crate::Cluster`] ranks.
pub trait InfoClient: rpc::Client {
    /// The node's newest block (or slot).
    fn tip(&self) -> Future<'_, anyhow::Result<u64>>;

    /// Its chain and tip.
    ///
    /// # Errors
    ///
    /// A node failure, or a node of another chain than configured.
    fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>>;
}

/// What a node says about itself: which chain it serves and its tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// The chain it serves, as the network's `chain_id` writes it (EVM chain
    /// ID, Solana genesis hash, Bitcoin Core chain name).
    pub chain_id: String,
    /// Its newest block (or slot).
    pub tip: u64,
}
