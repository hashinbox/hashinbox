//! [`Served`]: a chain over a list of blocks.

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use super::lock;
use crate::{Block, Chained, Dated, Future, Line, Network, Networked, NotServed, Tips};

/// A chain serving a list of blocks: its tip is the last one's position (0
/// with none), positions without a block are skipped, and a block's time
/// is its header's. It is final `confirmations` below the tip, its line is
/// no line by default, and once pruned it no longer serves the blocks below
/// a position. Clones share what was pruned and read.
#[derive(Clone)]
pub struct Served {
    network: Network,
    blocks: Arc<Vec<Block>>,
    confirmations: u64,
    line: Line,
    first: Arc<AtomicU64>,
    reads: Arc<Mutex<BTreeSet<u64>>>,
}

impl Served {
    /// A chain serving `blocks`, ascending by position, final
    /// `confirmations` below its tip.
    ///
    /// # Panics
    /// When `blocks` is empty: [`Served::empty`] has no block.
    #[must_use]
    pub fn new(blocks: Vec<Block>, confirmations: u64) -> Self {
        let network = blocks.first().expect("a block").network.clone();
        Self {
            blocks: Arc::new(blocks),
            confirmations,
            ..Self::empty(network)
        }
    }

    /// A chain of `network` that has produced nothing.
    #[must_use]
    pub fn empty(network: Network) -> Self {
        Self {
            network,
            blocks: Arc::default(),
            confirmations: 0,
            line: Line::Depth(u64::MAX),
            first: Arc::default(),
            reads: Arc::default(),
        }
    }

    /// The same chain, its line `line`.
    #[must_use]
    pub const fn with_line(mut self, line: Line) -> Self {
        self.line = line;
        self
    }

    /// From now on, no block below `below` is served ([`NotServed`]).
    pub fn prune(&self, below: u64) {
        self.first.store(below, Ordering::SeqCst);
    }

    /// Every position a served block was read at.
    #[must_use]
    pub fn reads(&self) -> BTreeSet<u64> {
        lock(&self.reads).clone()
    }
}

impl Networked for Served {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl Chained for Served {
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
        let position = self.blocks.last().map_or(0, |block| block.header.position);
        Box::pin(async move { Ok(Tips { position }) })
    }

    fn confirmations(&self) -> u64 {
        self.confirmations
    }

    fn line(&self) -> Line {
        self.line
    }

    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
        if position < self.first.load(Ordering::SeqCst) {
            return Box::pin(async move { Err(NotServed { position }.into()) });
        }
        lock(&self.reads).insert(position);
        let block = self
            .blocks
            .binary_search_by_key(&position, |block| block.header.position)
            .ok()
            .map(|at| self.blocks[at].clone());
        Box::pin(async move { Ok(block) })
    }
}

impl Dated for Served {}
