//! The Bitcoin indexer: [`indexer`] over [`IndexerParams`].

use std::time::Duration;

use indexing::{Budget, Config, IndexError, Indexer, Schedule};

use crate::{BlockInterpreter, RpcClient, Source};

/// Everything one Bitcoin indexer needs. Fields are public; [`Self::new`]
/// fills the chain defaults.
pub struct IndexerParams<C, R> {
    /// Verified client. Clone it to share with [`crate::WalletFactory`].
    pub rpc: RpcClient<C>,
    /// The one store this index registers observations in and commits to. It
    /// serves every network, so the same value is handed to every chain.
    pub repository: R,
    /// Defaults to [`IndexerParams::SYNC`].
    pub sync: Budget,
    /// How often a driver polls this chain. Defaults to [`IndexerParams::POLL`].
    pub schedule: Schedule,
    /// How many blocks below the node's tip the confirmed line sits.
    /// Defaults to [`IndexerParams::CONFIRMED_DEPTH`].
    pub confirmed_depth: u64,
}

impl<C, R> IndexerParams<C, R> {
    /// Batches of a hundred blocks.
    pub const SYNC: Budget = Budget::new(100);

    /// Ten seconds: a new tip is seen well within Bitcoin's ten-minute block.
    pub const POLL: Schedule = Schedule::every(Duration::from_secs(10));

    /// Blocks held above the confirmed line by default.
    ///
    /// Bitcoin has no finality, so this number *is* the safety policy, and it
    /// is a trade: at depth six, history and spendable balance appear about an
    /// hour after a block is mined, and a reorg deeper than six quarantines
    /// the network instead of repairing itself. Six is the settlement depth
    /// the industry already uses, and it is deeper than any Bitcoin mainnet
    /// reorg since the March 2013 consensus bug (24 blocks, a software defect
    /// rather than competing work); mainnet has seen no reorg of three blocks
    /// or more since. A hundred reproduces the old rollback journal's safety
    /// envelope exactly, at roughly sixteen hours of latency.
    pub const CONFIRMED_DEPTH: u64 = 6;

    /// The deepest confirmed line this indexer accepts.
    ///
    /// Every block above the line is held in memory and folded by every read,
    /// so an unbounded depth is both an unbounded working set and an
    /// unboundedly stale confirmed balance. A thousand blocks is about a week
    /// of Bitcoin: past it the value is a mistake, not a policy.
    pub const DEPTH_LIMIT: u64 = 1_000;

    /// Chain-default synchronization.
    #[must_use]
    pub const fn new(rpc: RpcClient<C>, repository: R) -> Self {
        Self {
            rpc,
            repository,
            sync: Self::SYNC,
            schedule: Self::POLL,
            confirmed_depth: Self::CONFIRMED_DEPTH,
        }
    }

    /// Rejects a confirmed depth of zero or one beyond [`Self::DEPTH_LIMIT`].
    fn validate_depth(&self) -> Result<(), IndexError> {
        if self.confirmed_depth == 0 || self.confirmed_depth > Self::DEPTH_LIMIT {
            return Err(IndexError::new(
                indexing::IndexErrorKind::InvalidRequest,
                format!(
                    "Bitcoin confirmation depth must be between 1 and {}",
                    Self::DEPTH_LIMIT
                ),
                false,
            ));
        }
        Ok(())
    }
}

/// Builds the indexer `params` describes, for the client's own network. No
/// I/O: the client was verified when it connected.
///
/// `indexing::Indexer` itself, over this chain's block source and interpreter,
/// rather than a newtype that forwarded every capability to one. Store it as
/// `Arc<dyn indexing::Indexing>` or combine several in an `indexing::Index`.
/// Unlike an account chain it also materializes UTXOs. Reading them is the
/// repository's (`indexing::OutputStore`), and `crate::sender::Spendable` folds
/// this index's pending blocks into those rows so a wallet can spend its own
/// change at depth one; `crate::WalletFactory` takes it as its
/// `Arc<dyn crate::sender::UtxoSource>`. Not an impl on the index, because the rows
/// are the store's: what the index adds is the half above its own confirmed
/// line.
///
/// A function rather than an inherent constructor, because this crate does
/// not own [`Indexer`]; [`IndexerParams`] stays what it is, the argument,
/// rather than becoming a factory for its own product.
///
/// # Errors
///
/// `InvalidRequest` for an invalid [`Budget`] or confirmation depth.
pub fn indexer<C, R>(
    params: IndexerParams<C, R>,
) -> Result<Indexer<Source<C>, BlockInterpreter, R>, IndexError> {
    params.validate_depth()?;
    let network = *params.rpc.network();
    let depth = params.confirmed_depth;
    Ok(Indexer::new(
        Source::new(params.rpc, depth),
        BlockInterpreter::new(&network)?,
        params.repository,
        Config::new(&network, params.sync, params.schedule)?,
    ))
}
