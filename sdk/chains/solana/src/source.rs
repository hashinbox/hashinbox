//! [`indexing::BlockSource`] over the verified RPC client; [`RawBlock`] as the
//! [`indexing::NativeBlock`]. All node access goes through `native::rpc`.

use base::{BlockPosition, BlockRef, BoxFuture};
use indexing::{BlockSource, ChainTips, NativeBlock, SourceError};

use crate::native::{block::RawBlock, rpc::Client};

/// The indexer's block source: finalized produced blocks over sparse slots.
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

    /// The same block as [`Self::tip`]. Every index-path read this source
    /// makes already carries `{"commitment":"finalized"}`, so its tip is
    /// already rooted: a rooted slot is locked out by a supermajority and
    /// cannot be replaced. Solana therefore has no pending region at all, and
    /// carries no depth number.
    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        self.tip()
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
                    "Solana block range requires ordered positions and a positive limit",
                    false,
                ));
            }
            Ok(self.rpc.blocks(start, end, limit).await?)
        })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        Box::pin(async move { Ok(self.rpc.canonical_at(position).await?) })
    }
}
