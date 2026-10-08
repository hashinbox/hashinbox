//! [`indexing::BlockSource`] over the verified RPC client; [`RawBlock`] as the
//! [`indexing::NativeBlock`]. All node access goes through `native::rpc`.

use base::{BlockPosition, BlockRef, BoxFuture};
use indexing::{BlockSource, ChainTips, NativeBlock, SourceError};

use crate::native::{Error, block::RawBlock, rpc::Client};

/// The indexer's block source: numbered blocks with every receipt.
///
/// Built by [`crate::indexer`] from an already verified client,
/// so it performs no identity check of its own.
pub struct Source<C> {
    rpc: Client<C>,
}

impl<C> Source<C> {
    /// A source over an already verified client.
    #[must_use]
    pub const fn new(rpc: Client<C>) -> Self {
        Self { rpc }
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

    /// The node's own `finalized` block. Ethereum states its line itself, so
    /// this source carries no depth number at all.
    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move { Ok(self.rpc.confirmed_tip().await?) })
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
                    "Ethereum block range requires ordered positions and a positive limit",
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
