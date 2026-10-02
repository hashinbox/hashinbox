//! How a pass asks for blocks, and what it checks before believing them.
//!
//! Shared by both phases: the forward one reads towards the tip and the
//! backfill one reads below the checkpoint, and neither may accept a source
//! that returns more blocks than it asked for, blocks outside the range, or
//! blocks that do not strictly ascend. A source is remote and a pass commits
//! what it returns, so the checks are here rather than in each caller.
//!
//! Fetching is also the only thing that ever wants a *native* block twice, so
//! the cache of those ([`NativeBlocks`]) is here and nowhere else: the
//! interpreted facts above the confirmed line ([`crate::pending::Blocks`])
//! are this crate's own types and are kept unconditionally, while the chain's
//! raw blocks are a bounded, discardable head start on a refetch.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{
    BlockInterpreter, BlockPosition, BlockRef, BlockSource, IndexError, IndexErrorKind, Indexer,
    NativeBlock,
};

/// How many of the newest native blocks are kept.
///
/// A native block is only ever needed again to re-inspect a block after an
/// address was added, and it is megabytes where
/// the interpreted facts are usually bytes. Keeping a short suffix makes the
/// ordinary case free and leaves the rest to one bounded refetch.
const RETAINED_BLOCKS: usize = 16;

/// The newest native blocks this pass fetched, newest last.
///
/// Pure memoization of [`Indexer::one_block`]: every entry can be thrown
/// away and refetched, which is exactly what falling out of the window does.
/// Entries are keyed by the whole [`BlockRef`], so a pending block that was
/// unwound can never be handed back for the hash that replaced it; it just
/// ages out unread.
pub(crate) struct NativeBlocks<B> {
    blocks: Mutex<VecDeque<CachedBlock<B>>>,
}

/// One native block, and which block it was read for.
struct CachedBlock<B> {
    block: BlockRef,
    raw: Arc<B>,
}

impl<B> Default for NativeBlocks<B> {
    fn default() -> Self {
        Self {
            blocks: Mutex::new(VecDeque::new()),
        }
    }
}

impl<B> NativeBlocks<B> {
    pub(crate) fn clear(&self) -> Result<(), IndexError> {
        self.entries()?.clear();
        Ok(())
    }

    fn entries(&self) -> Result<MutexGuard<'_, VecDeque<CachedBlock<B>>>, IndexError> {
        self.blocks.lock().map_err(|_| {
            IndexError::new(
                IndexErrorKind::Store,
                "native block cache is poisoned",
                false,
            )
        })
    }

    /// Keeps `raw` as the block `fetched` was read from, dropping whatever
    /// fell out of the window.
    pub(super) fn keep(&self, fetched: BlockRef, raw: Arc<B>) -> Result<(), IndexError> {
        let mut entries = self.entries()?;
        entries.push_back(CachedBlock {
            block: fetched,
            raw,
        });
        while entries.len() > RETAINED_BLOCKS {
            entries.pop_front();
        }
        Ok(())
    }

    /// Takes the native block `wanted` was read from, if it is still held.
    fn take(&self, wanted: &BlockRef) -> Result<Option<Arc<B>>, IndexError> {
        let mut entries = self.entries()?;
        let found = entries.iter().position(|entry| entry.block == *wanted);
        Ok(found
            .and_then(|index| entries.remove(index))
            .map(|entry| entry.raw))
    }
}

impl<S, I, R> Indexer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
{
    /// The native block behind a pending block's reference, refetched
    /// when the cache no longer holds it.
    pub(super) async fn refetch(&self, block: BlockRef) -> Result<I::Block, IndexError> {
        match self.native.take(&block)? {
            Some(raw) => Ok(Arc::unwrap_or_clone(raw)),
            None => self.one_block(block.position, block.position).await,
        }
    }

    pub(super) async fn one_block(
        &self,
        start: BlockPosition,
        end: BlockPosition,
    ) -> Result<I::Block, IndexError> {
        let blocks = self.fetch_blocks(start, end, 1).await?;
        let [block]: [I::Block; 1] = blocks.try_into().map_err(|_| {
            IndexError::disconnected("source did not return the required produced block")
        })?;
        Ok(block)
    }

    pub(super) async fn fetch_blocks(
        &self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> Result<Vec<I::Block>, IndexError> {
        let blocks = self
            .source
            .blocks(start, end, limit)
            .await
            .map_err(IndexError::from)?;
        if blocks.len() > limit {
            return Err(IndexError::invalid_answer(
                "source exceeded the returned-block limit",
            ));
        }
        let mut previous = None;
        for block in &blocks {
            let position = block.block_ref().position;
            let invalid_position = position < start
                || position > end
                || previous.is_some_and(|previous| position <= previous);
            if invalid_position {
                return Err(IndexError::invalid_answer(
                    "source blocks are outside the range or not strictly increasing",
                ));
            }
            previous = Some(position);
        }
        Ok(blocks)
    }
}

#[cfg(test)]
mod fetch_test {
    use super::*;
    use crate::{BlockHash, BlockHeight};

    /// A native block stands in as one number: the cache never reads one.
    fn block(position: u64, hash: u8) -> BlockRef {
        BlockRef {
            position: BlockPosition(position),
            height: BlockHeight(position),
            hash: BlockHash(vec![hash]),
            parent: None,
            timestamp: None,
        }
    }

    #[test]
    fn a_kept_block_is_handed_back_once_and_then_refetched() {
        let cache = NativeBlocks::default();
        cache
            .keep(block(7, 7), Arc::new(7_u64))
            .expect("the cache keeps it");
        assert_eq!(
            cache.take(&block(7, 7)).expect("a hit"),
            Some(Arc::new(7_u64))
        );
        assert_eq!(
            cache.take(&block(7, 7)).expect("a miss"),
            None,
            "taking it empties the entry, so a second pass refetches"
        );
    }

    #[test]
    fn only_the_newest_blocks_are_kept() {
        let cache = NativeBlocks::default();
        for position in 0..=u64::try_from(RETAINED_BLOCKS).expect("a small window") {
            let hash = u8::try_from(position).expect("a small window");
            cache
                .keep(block(position, hash), Arc::new(position))
                .expect("the cache keeps it");
        }
        assert_eq!(
            cache.take(&block(0, 0)).expect("the oldest"),
            None,
            "the oldest block fell out of the window"
        );
        assert!(
            cache.take(&block(1, 1)).expect("the next").is_some(),
            "everything newer is still held"
        );
    }

    #[test]
    fn a_block_of_another_hash_at_the_same_position_is_not_handed_back() {
        let cache = NativeBlocks::default();
        cache
            .keep(block(9, 1), Arc::new(9_u64))
            .expect("the cache keeps it");
        assert_eq!(
            cache.take(&block(9, 2)).expect("a reorged position"),
            None,
            "an unwound block's bytes never answer for the block that replaced it"
        );
    }
}
