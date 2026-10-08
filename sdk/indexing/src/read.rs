//! **Action.**
//!
//! One reading of the chain, and the whole of what every reading checks before
//! it believes a block.
//!
//! Both halves of a pass read a stretch of chain and turn each block into this
//! network's facts: [`catchup`](crate::run) reads towards the observed tip,
//! and [`backfill`](crate::run) reads the blocks below the checkpoint that
//! an observation watched from its own past still owes. What they *check* is the
//! same, so it is here once rather than once per direction — two copies of it is
//! how a historical block came to be accepted under weaker rules than a
//! committed one (`docs/DEFECT_PATTERNS.md` section 1.2).
//!
//! The counterpart of [`fetch`](crate::read), which checks what one *request*
//! came back with: no more blocks than were asked for, none outside the range,
//! and strictly ascending positions. Those are properties of one answer, and a
//! reading's continuity is not — the forward half validates each block against
//! whatever is already held above the line, including on a re-inspection that
//! fetched nothing at all — so continuity is a cursor threaded through the
//! reading ([`Scan`]) rather than a check inside a fetch.
//!
//! What stays per direction is *scheduling* and nothing else: the forward half
//! runs first and spends the batch budget first, so a historical range this node
//! cannot serve can never delay the tip every client reads.

use crate::block::extends;
use crate::{
    Address, BlockInterpreter, BlockPosition, BlockRef, BlockSource, BlockWithOutputs, IndexError,
    IndexErrorKind, Indexer, NativeBlock, Plan, pending,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

/// One reading of the chain in progress: the block the next one must follow.
///
/// A reading is contiguous by construction, and that is what lets a single
/// [`BlockRef`] say how far anything has been inspected
/// ([`crate::WatchProgress::indexed_through`]): every block is checked against
/// the one before it, and the first against whatever the reading resumes after —
/// the newest block held above the line, the network's checkpoint, or the block
/// one observation last recorded for itself.
pub(crate) struct Scan {
    /// The block read last, which the next one must follow.
    previous: Option<BlockRef>,
}

impl Scan {
    /// A reading that resumes after `previous`, or one whose first block has
    /// nothing here to follow when it is `None`.
    pub(crate) const fn after(previous: Option<BlockRef>) -> Self {
        Self { previous }
    }

    /// Checks that `block` follows what this reading has read, and takes it over
    /// as the block the next one must follow.
    ///
    /// Produced blocks are adjacent by *height* and linked by parent, whatever
    /// the positions in between: Solana's positions are slots and most hold no
    /// block, so a gap between two positions is not a gap in the reading
    /// ([`crate::WatchProgress::next_required`]).
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::Disconnected`] for a block that does not
    /// follow, a retryable [`IndexErrorKind::InvalidBlock`] for a reference
    /// no chain can produce (a parent on genesis, none above it), and a
    /// terminal one when the previous block's height has no successor.
    pub(crate) fn follows(&mut self, block: &BlockRef) -> Result<(), IndexError> {
        if block.position == BlockPosition(0) {
            if block.parent.is_some() {
                return Err(IndexError::invalid_answer(
                    "genesis block must not have a parent",
                ));
            }
        } else if block.parent.is_none() {
            return Err(IndexError::invalid_answer(
                "non-genesis block is missing its parent",
            ));
        }
        if let Some(read) = &self.previous
            && !extends(block, read)?
        {
            return Err(IndexError::disconnected(
                "source block does not follow the block before it",
            ));
        }
        self.previous = Some(block.clone());
        Ok(())
    }
}

impl<S, I, R> Indexer<S, I, R>
where
    S: BlockSource<Block = I::Block>,
    I: BlockInterpreter,
{
    /// Whether the source still calls `block` canonical: what it states at that
    /// position is this block, in every field.
    ///
    /// **The one question a reorg is detected by.** It is asked of the committed
    /// checkpoint, of the newest pending block and of a block about to be held,
    /// and what a "no" means is each caller's own: a confirmed reorg that only a
    /// rescan repairs, a pending block to discard, a block to read again. A
    /// position the source states nothing at is a "no" as well.
    ///
    /// # Errors
    ///
    /// The source's own error, with its own retryability.
    pub(super) async fn is_canonical(&self, block: &BlockRef) -> Result<bool, IndexError> {
        let stated = self.source.canonical_at(block.position).await?;
        Ok(stated.as_ref() == Some(block))
    }

    /// The next block of one reading, inspected for `addresses` and validated as
    /// this network's facts.
    ///
    /// **The single inspection procedure**, and the whole of it: the block
    /// follows the reading so far, the interpreter did not change which block it
    /// read, and what it produced is something this network can hold — no
    /// overlapping output change, no two transactions at one native place, no
    /// movement or credit of another network ([`BlockWithOutputs::new`]). A
    /// transaction identity given twice never reaches that validation: the
    /// interpreter's own output cannot be assembled with one
    /// ([`crate::BlockWithOutputs::new`]). One validation point, reached from both
    /// directions, so neither can be given a rule the other lacks.
    ///
    /// What is deliberately *not* here is the source's own canonicality. Only
    /// blocks above the confirmed line can still be taken back, and only
    /// [`Self::admit`] reads any; a historical block is at or below a checkpoint
    /// this pass already proved canonical, and a confirmed block never reverts, so asking again
    /// would be one RPC per
    /// block for no further proof.
    pub(super) fn inspect(
        &self,
        scan: &mut Scan,
        block: &I::Block,
        addresses: &[Address],
    ) -> Result<BlockWithOutputs, IndexError> {
        let reference = block.block_ref();
        scan.follows(&reference)?;
        let interpreted = self.interpreter.inspect(block, addresses)?;
        interpreted.validate_reference(&reference)?;
        interpreted.validate(&self.config.network)?;
        Ok(interpreted)
    }

    /// Inspects one block and holds it above the confirmed line, in `pending`.
    ///
    /// Which collection is an argument because it is not always the shared one:
    /// [`Indexer::reinspect`](Self::reinspect) admits into a private collection
    /// and swaps it in afterwards, so a read never sees a half-rebuilt one. That
    /// collection is also the forward reading's own cursor — a block follows
    /// whatever is held, or the checkpoint when nothing is — so the [`Scan`] is
    /// taken from it here rather than carried across calls.
    ///
    /// Nothing durable happens here, and nothing written down says where the
    /// block will go: it is inspected by [`Self::inspect`], checked one last time
    /// against the source that served it, and kept as facts.
    pub(super) async fn admit(
        &self,
        pending: &pending::Blocks,
        source_block: I::Block,
        addresses: &[Address],
        plan: &Plan,
    ) -> Result<(), IndexError> {
        let block = source_block.block_ref();
        let mut scan = Scan::after(pending.tip()?.or_else(|| plan.checkpoint().cloned()));
        let canonical = self.inspect(&mut scan, &source_block, addresses)?;
        if !self.is_canonical(&block).await? {
            return Err(IndexError::disconnected(
                "source block changed before commit",
            ));
        }
        self.native.keep(block, Arc::new(source_block))?;
        pending.admit(canonical, plan.selection())
    }
}

// How a pass asks for blocks, and what it checks before believing them.
//
// Shared by both phases: the forward one reads towards the tip and the
// backfill one reads below the checkpoint, and neither may accept a source
// that returns more blocks than it asked for, blocks outside the range, or
// blocks that do not strictly ascend. A source is remote and a pass commits
// what it returns, so the checks are here rather than in each caller.
//
// Fetching is also the only thing that ever wants a *native* block twice, so
// the cache of those ([`Cache`]) is here and nowhere else: the
// interpreted facts above the confirmed line ([`crate::pending::Blocks`])
// are this crate's own types and are kept unconditionally, while the chain's
// raw blocks are a bounded, discardable head start on a refetch.

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
pub(crate) struct Cache<B> {
    blocks: Mutex<VecDeque<CachedBlock<B>>>,
}

/// One native block, and which block it was read for.
struct CachedBlock<B> {
    block: BlockRef,
    raw: Arc<B>,
}

impl<B> Default for Cache<B> {
    fn default() -> Self {
        Self {
            blocks: Mutex::new(VecDeque::new()),
        }
    }
}

impl<B> Cache<B> {
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
        let cache = Cache::default();
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
        let cache = Cache::default();
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
        let cache = Cache::default();
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
