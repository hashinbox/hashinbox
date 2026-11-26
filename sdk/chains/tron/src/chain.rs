//! [`Chain`]: Tron blocks as `base` blocks, confirmed `confirmations` below
//! the tip.

use base::{Block, Blocks, Chained, Dated, Future, Line, Network, Networked, Tips};

use crate::{Client, Tokens};

/// One Tron network read from its node. Position and height are both the
/// block number (Tron skips none); a block is confirmed once
/// `confirmations` below the tip (19, about where the solidified block
/// sits), and nothing more than `max_depth` below the tip is read. Legs are
/// of TRX and of the allowlisted tokens only.
pub struct Chain {
    rpc: Client,
    network: Network,
    tokens: Tokens,
    confirmations: u64,
    max_depth: Option<u64>,
}

impl Chain {
    /// Reads over `rpc`, indexing `tokens`, a block confirmed
    /// `confirmations` below the tip, nothing more than `max_depth` below
    /// it (`None`: no limit).
    #[must_use]
    pub fn new(rpc: Client, tokens: Tokens, confirmations: u64, max_depth: Option<u64>) -> Self {
        let network = rpc.network().into();
        Self {
            rpc,
            network,
            tokens,
            confirmations,
            max_depth,
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

    fn confirmations(&self) -> u64 {
        self.confirmations
    }

    /// Tron skips no number: a block the node does not hold yet is an
    /// error, not `None`.
    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
        Box::pin(async move {
            let block = self.blocks(position, position).await?.first().cloned();
            anyhow::ensure!(
                block.is_some(),
                "Tron block {position} is not available yet"
            );
            Ok(block)
        })
    }

    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(
            self.rpc
                .blocks(start, end, &self.tokens, self.confirmations),
        )
    }

    /// The tip less `max_depth`; none set: no line. Tron reads no balance
    /// at a past block, so an index is never settled, only read.
    fn line(&self) -> Line {
        Line::Depth(self.max_depth.unwrap_or(u64::MAX))
    }
}

/// Finds blocks by time through header-only reads.
impl Dated for Chain {
    fn time(&self, position: u64) -> Future<'_, anyhow::Result<Option<u64>>> {
        Box::pin(async move { Ok(Some(self.rpc.time(position).await?)) })
    }
}
