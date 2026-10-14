//! [`Chain`]: Bitcoin blocks as `base` blocks, final at a configured depth.

use anyhow::ensure;
use base::{Block, Blocks, Chained, Future, Network, Networked, Tips};

use crate::Client;

/// One Bitcoin network read from its node. Position and height are both the
/// block height; a block is final `depth` blocks deep (Bitcoin has no
/// finality of its own).
pub struct Chain {
    rpc: Client,
    network: Network,
    depth: u64,
}

impl Chain {
    /// Confirmed blocks `depth` deep: `depth` 1 is the tip itself.
    ///
    /// # Errors
    ///
    /// For a depth outside `1..=1000`.
    pub fn new(rpc: Client, depth: u64) -> anyhow::Result<Self> {
        ensure!(
            (1..=1_000).contains(&depth),
            "Bitcoin confirmation depth must be between 1 and 1000"
        );
        let network = rpc.network().into();
        Ok(Self {
            rpc,
            network,
            depth,
        })
    }
}

impl Networked for Chain {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl Chained for Chain {
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
        Box::pin(self.rpc.tips(self.depth))
    }

    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
        Box::pin(async move {
            let run = self.rpc.blocks(position, position).await?;
            Ok(run.first().cloned())
        })
    }

    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(self.rpc.blocks(start, end))
    }
}
