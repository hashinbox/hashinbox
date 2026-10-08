//! [`indexing::BlockSource`] over the verified RPC client; [`RawBlock`] as the
//! [`indexing::NativeBlock`]. All node access goes through `native::rpc`.

use base::{BlockPosition, BlockRef, BoxFuture};
use indexing::{BlockSource, ChainTips, NativeBlock, SourceError};

use crate::native::{Error, block::RawBlock, rpc::Client};

/// The indexer's block source: numbered blocks with every spent output resolved.
///
/// Built by [`crate::indexer`] from an already verified client,
/// so it performs no identity check of its own.
///
/// `depth` is the only number Bitcoin carries about finality, because Bitcoin
/// has none: the confirmed line is `tip - depth`, and a reorg deeper than that
/// is a terminal failure rather than something the index repairs. See
/// [`crate::IndexerParams::CONFIRMED_DEPTH`].
pub struct Source<C> {
    rpc: Client<C>,
    depth: u64,
}

impl<C> Source<C> {
    /// A source over an already verified client, confirming at `depth` blocks
    /// below the tip.
    #[must_use]
    pub const fn new(rpc: Client<C>, depth: u64) -> Self {
        Self { rpc, depth }
    }
}

impl NativeBlock for RawBlock {
    fn block_ref(&self) -> BlockRef {
        self.reference().clone()
    }
}

impl<C: json_rpc::Client> ChainTips for Source<C> {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move { Ok(self.rpc.tip().await?) })
    }

    /// `tip - depth`, resolved through the same hash and header reads `tip`
    /// makes.
    ///
    /// A chain shorter than `depth` confirms at genesis, which is the only
    /// block it can state is irreversible without guessing. Bitcoin's native
    /// position is its produced height, so the subtraction is exact.
    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let tip = self.rpc.tip().await?;
            let confirmed = BlockPosition(tip.height.0.saturating_sub(self.depth));
            if confirmed == tip.position {
                return Ok(tip);
            }
            self.rpc
                .canonical_at(confirmed)
                .await?
                .ok_or_else(Error::missing_block)
                .map_err(SourceError::from)
        })
    }
}

impl<C: json_rpc::Client> BlockSource for Source<C> {
    type Block = RawBlock;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<RawBlock>, SourceError>> {
        Box::pin(async move {
            if limit == 0 || start > end {
                return Err(SourceError::new(
                    "Bitcoin block range requires ordered positions and a positive limit",
                    false,
                ));
            }
            let mut blocks = Vec::with_capacity(limit.min(64));
            let mut position = Some(start);
            while let Some(current) = position.filter(|current| *current <= end) {
                if blocks.len() == limit {
                    break;
                }
                let block = self.rpc.block(current).await?;
                blocks.push(block.ok_or_else(Error::missing_block)?);
                position = current.checked_successor();
            }
            Ok(blocks)
        })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        Box::pin(async move { Ok(self.rpc.canonical_at(position).await?) })
    }
}
