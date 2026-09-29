//! What a chain must implement to be indexed: `IndexedBlock` (a native block
//! that can state its `BlockRef`), `ChainTip` (the produced and confirmed
//! lines), `BlockSource` (reading blocks) and `BlockInterpreter` (turning one
//! block into an `ObservationDraft`). Four traits, no implementations.

use crate::{
    Address, BlockPosition, BlockRef, BoxFuture, IndexError, InterpretedBlock, SourceError,
};

/// Gives generic synchronization access to a chain-native block identity.
pub trait IndexedBlock: Clone + Send + Sync + 'static {
    /// The block's position, height, hash, and parent. Must be identical for
    /// the same native block every time, since synchronization compares it
    /// with [`BlockSource::canonical_at`] before and after interpretation.
    fn block_ref(&self) -> BlockRef;
}

/// The two lines a chain states about itself: how far it has produced, and
/// how far it will never take back.
///
/// Both are read once per pass. The distance between them is the whole of
/// [`crate::Service`]'s pending overlay: blocks at or below the confirmed tip
/// are committed to durable storage, blocks above it are held in memory
/// (docs/design/030 §8 (ex-025)).
pub trait ChainTip: Send + Sync {
    /// The newest produced block the source considers canonical. A pass
    /// reads up to this tip, never beyond it.
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;

    /// The newest block this source will state is irreversible. Never above
    /// [`Self::tip`], and never moving backwards.
    ///
    /// This is the line the durable store commits at, so no stored row is ever
    /// undone. There is deliberately no default implementation, because the
    /// answer is a property of the chain's consensus and each one must state
    /// its own: Ethereum asks its node for the `finalized` block, Solana
    /// already reads only rooted blocks and returns its own tip, and Bitcoin,
    /// which has no finality, subtracts a configured depth and accepts a
    /// probabilistic line.
    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;
}

/// Fetches canonical chain data. Concrete RPC methods remain in the chain crate.
///
/// Every error is a [`SourceError`]; its `retryable` flag decides whether the
/// supervisor tries the pass again or quarantines
/// that one network.
pub trait BlockSource: ChainTip {
    /// The chain-native block this source fetches.
    type Block: IndexedBlock;

    /// Returns at most `limit` actual produced blocks in the inclusive range,
    /// in strictly increasing position order. Positions without a produced
    /// block (Solana's skipped slots) are omitted, not returned empty.
    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>>;

    /// Returns the complete canonical reference at one native position, or
    /// `None` when no produced block is canonical there. Used to detect reorgs.
    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>>;
}

/// Converts a chain-native block into relevant canonical changes.
///
/// Pure and synchronous: it sees one block and the addresses active at its
/// position, and never touches storage or the network.
pub trait BlockInterpreter: Send + Sync {
    /// The chain-native block this interpreter reads; matches the source's.
    type Block: IndexedBlock;

    /// Extracts the transactions and output changes that touch `addresses`.
    ///
    /// The returned [`InterpretedBlock::block`] must equal
    /// [`IndexedBlock::block_ref`]; synchronization rejects anything else as
    /// [`IndexErrorKind::InvalidBlock`](crate::IndexErrorKind::InvalidBlock).
    /// Malformed native content is also `InvalidBlock` (terminal).
    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError>;

    /// Where this chain's network fee already sits, relative to the movements
    /// [`Self::inspect`] emits.
    ///
    /// There is deliberately no default: a balance fold that guesses wrong is
    /// wrong by exactly one fee on every transaction the holding paid for.
    /// The interpreter answers it because the interpreter is what produced
    /// both the movements and the [`NetworkFee`](crate::NetworkFee), so it is
    /// the one place that knows whether they overlap.
    fn settlement(&self) -> crate::FeeSettlement;
}
