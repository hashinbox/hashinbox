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

#[cfg(test)]
mod tests {
    use futures_executor::block_on;

    use serde_json::json;

    use super::*;
    use crate::native::rpc::script::{Reply, Script, Scripted};

    #[test]
    fn invalid_ranges_fail_before_rpc_and_missing_blocks_are_retryable() {
        let script = Script::node(vec![Reply::failure("getblockhash", -8, "out of range")]);
        let source = Source::new(script.client(), 6);
        for (start, end, limit) in [(10, 10, 0), (11, 10, 1)] {
            let error = block_on(source.blocks(BlockPosition(start), BlockPosition(end), limit))
                .unwrap_err();
            assert!(!error.retryable);
        }
        let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1)).unwrap_err();
        assert_eq!(
            error.message,
            "Bitcoin Core does not currently expose the requested block"
        );
        assert!(error.retryable);
        script.exhausted();
    }

    /// The confirmed line is the tip minus the configured depth, resolved
    /// through the same hash and header reads the tip itself makes. A chain
    /// shorter than the depth confirms at genesis.
    #[test]
    fn the_confirmed_tip_is_the_configured_depth_below_the_tip() {
        for (tip, depth, expected) in [(100_u64, 6_u64, 94_u64), (3, 6, 0), (7, 7, 0)] {
            let script = Script::node(vec![
                Reply::ok("getblockcount", json!(tip)),
                Reply::ok("getblockhash", json!(hash_text(tip))),
                Reply::ok("getblockheader", header(tip)),
                Reply::ok("getblockcount", json!(tip)),
                Reply::ok("getblockhash", json!(hash_text(expected))),
                Reply::ok("getblockheader", header(expected)),
            ]);
            let confirmed = block_on(Source::new(script.client(), depth).confirmed_tip())
                .expect("the confirmed tip");
            assert_eq!(confirmed.height, base::BlockHeight(expected));
            script.exhausted();
        }
    }

    /// The same source at depth zero would confirm its own tip; the depth is
    /// validated where it is configured, so this only pins that the
    /// subtraction is exact at the boundary.
    #[test]
    fn a_zero_depth_returns_the_tip_without_a_second_lookup() {
        let script = Script::node(vec![
            Reply::ok("getblockcount", json!(9_u64)),
            Reply::ok("getblockhash", json!(hash_text(9))),
            Reply::ok("getblockheader", header(9)),
        ]);
        let confirmed =
            block_on(Source::new(script.client(), 0).confirmed_tip()).expect("the tip itself");
        assert_eq!(confirmed.height, base::BlockHeight(9));
        script.exhausted();
    }

    fn hash_text(height: u64) -> String {
        format!("{height:064x}")
    }

    fn header(height: u64) -> serde_json::Value {
        let mut value = serde_json::json!({
            "hash": hash_text(height),
            "height": height,
            "time": 1_000 + height,
        });
        if height > 0 {
            value["previousblockhash"] = json!(hash_text(height - 1));
        }
        value
    }
}
