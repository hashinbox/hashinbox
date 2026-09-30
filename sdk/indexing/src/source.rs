//! What a chain must implement to be indexed: `NativeBlock` (a native block
//! that can state its `BlockRef`), `ChainTips` (the produced and confirmed
//! lines), `BlockSource` (reading blocks) and `BlockInterpreter` (turning one
//! block into an `ObservationDraft`). Four traits, no implementations, and the
//! one answer an interpreter gives about its chain: `FeeSettlement`.

use crate::{
    Address, BlockPosition, BlockRef, BoxFuture, IndexError, InterpretedBlock, SourceError,
};

/// Gives generic synchronization access to a chain-native block identity.
pub trait NativeBlock: Clone + Send + Sync + 'static {
    /// The block's position, height, hash, and parent. Must be identical for
    /// the same native block every time, since synchronization compares it
    /// with [`BlockSource::canonical_at`] before and after interpretation.
    fn block_ref(&self) -> BlockRef;
}

/// The two lines a chain states about itself: how far it has produced, and
/// how far it will never take back.
///
/// Both are read once per pass. The distance between them is the whole of
/// [`crate::Indexer`]'s pending blocks: blocks at or below the confirmed tip
/// are committed to durable storage, blocks above it are held in memory.
pub trait ChainTips: Send + Sync {
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
pub trait BlockSource: ChainTips {
    /// The chain-native block this source fetches.
    type Block: NativeBlock;

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

/// Where a chain settles its network fee, relative to the movements its
/// interpreter emits.
///
/// The distinction is not cosmetic and it is not guessable. On a UTXO chain
/// the interpreter emits every spent input and every created output, so
/// `Σ inputs − Σ outputs` has already paid the fee; subtracting
/// [`NetworkFee`](crate::NetworkFee) again would charge it twice. On an
/// account chain the fee is charged outside the transferred value and has to
/// be applied, to its payer and in the fee asset only — which is why a token
/// holding never pays gas, and the same address's native holding always does.
///
/// **A fact about the chain, stated by its interpreter**
/// ([`BlockInterpreter::settlement`]) and carried to the store with every
/// batch ([`crate::IndexedBatch::settlement`]). It is declared here, beside the
/// trait that answers it, because nothing about it is a query: a store reads
/// it when it writes, and whatever it folds afterwards is its own business.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeSettlement {
    /// Already inside the movements: nothing may apply it again.
    Embedded,
    /// Charged beside them: the payer is debited, in the fee asset only.
    Charged,
}

/// Converts a chain-native block into relevant canonical changes.
///
/// Pure and synchronous: it sees one block and the addresses active at its
/// position, and never touches storage or the network.
pub trait BlockInterpreter: Send + Sync {
    /// The chain-native block this interpreter reads; matches the source's.
    type Block: NativeBlock;

    /// Extracts the transactions and output changes that touch `addresses`.
    ///
    /// The reference of the returned [`InterpretedBlock::block`] must equal
    /// [`NativeBlock::block_ref`], and its network must be the one being
    /// synchronized; synchronization rejects anything else as
    /// [`IndexErrorKind::InvalidBlock`](crate::IndexErrorKind::InvalidBlock) or
    /// [`IndexErrorKind::NetworkMismatch`](crate::IndexErrorKind::NetworkMismatch).
    /// The output is assembled with [`InterpretedBlock::new`], which refuses
    /// two transactions under one identifier.
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
    fn settlement(&self) -> FeeSettlement;
}
