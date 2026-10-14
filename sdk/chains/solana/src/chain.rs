//! [`Chain`]: Solana blocks as `base` blocks, final once rooted.

use base::{Block, Blocks, Chained, Future, Network, Networked, Tips};

use crate::{Client, Tokens};

/// One Solana cluster read from its node at confirmed commitment. Position
/// is the slot (skipped slots hold no block), height the block height; a
/// block is final once rooted (`finalized`). Legs are of SOL and of the
/// allowlisted tokens only.
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

    /// `None` for a skipped slot.
    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
        Box::pin(async move { Ok(self.blocks(position, position).await?.first().cloned()) })
    }

    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(self.rpc.blocks(start, end, &self.tokens))
    }
}
