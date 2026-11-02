//! [`Chain`]: Ethereum blocks as `base` blocks, final once finalized.

use base::{Block, Blocks, Chained, Future, Network, Networked, Tips};

use crate::{Client, Tokens};

/// One Ethereum network read from its node. Position and height are both the
/// block number; a block is final once at or below the node's `finalized`
/// block. Legs are of ether and of the allowlisted tokens only; ether a
/// contract moves internally is not seen (standard RPC records it nowhere).
pub struct Chain {
    rpc: Client,
    network: Network,
    tokens: Tokens,
}

impl Chain {
    /// Reads over `rpc`, indexing `tokens`.
    #[must_use]
    pub fn new(rpc: Client, tokens: Tokens) -> Self {
        let network = rpc.network().into();
        Self {
            rpc,
            network,
            tokens,
        }
    }
}

impl Networked for Chain {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl Chained for Chain {
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
        Box::pin(self.rpc.tips())
    }

    /// Ethereum skips no height: a block the node does not hold yet is an
    /// error, not `None`.
    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
        Box::pin(async move {
            let block = self.blocks(position, position).await?.first().cloned();
            anyhow::ensure!(
                block.is_some(),
                "Ethereum block {position} is not available yet"
            );
            Ok(block)
        })
    }

    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(self.rpc.blocks(start, end, &self.tokens))
    }
}
