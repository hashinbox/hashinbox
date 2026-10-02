//! The Bitcoin indexer: [`indexer`] over [`IndexerParams`].

use indexing::{IndexError, Indexer, SyncConfig, SyncPolicy};

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
    pub sync: SyncPolicy,
    /// How many blocks below the node's tip the confirmed line sits.
    /// Defaults to [`IndexerParams::CONFIRMED_DEPTH`].
    pub confirmed_depth: u64,
}

impl<C, R> IndexerParams<C, R> {
    /// Batches of a hundred blocks.
    pub const SYNC: SyncPolicy = SyncPolicy::new(100);

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
/// repository's (`indexing::OutputStore`), and `indexing::PendingJoin` folds this
/// index's pending blocks into that page so a wallet can spend its own change at
/// depth one; `crate::WalletFactory` spends the result as its
/// `Arc<dyn indexing::OutputStore>`. Not an impl on the index, because the rows
/// are the store's: what the index adds is the half above its own confirmed
/// line.
///
/// A function rather than an inherent constructor, because this crate does
/// not own [`Indexer`]; [`IndexerParams`] stays what it is, the argument,
/// rather than becoming a factory for its own product.
///
/// # Errors
///
/// `InvalidRequest` for an invalid [`SyncPolicy`] or confirmation depth.
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
        SyncConfig::new(&network, params.sync)?,
    ))
}

#[cfg(test)]
mod tests {
    use base::Networked;
    use indexing::IndexErrorKind;

    use super::*;
    use crate::native::rpc::script::{Script, Scripted};

    #[test]
    fn construction_uses_the_client_network_without_rpc() {
        let script = Script::node(Vec::new());
        let index = indexer(IndexerParams::new(script.client(), ())).unwrap();
        assert_eq!(index.network(), &script.client().network().canonical());
        assert!(script.requests().is_empty());
    }

    #[test]
    fn construction_rejects_a_zero_batch_without_rpc() {
        let script = Script::node(Vec::new());
        let error = indexer(IndexerParams {
            sync: SyncPolicy::new(0),
            ..IndexerParams::new(script.client(), ())
        })
        .err()
        .unwrap();
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        assert!(script.requests().is_empty());
    }

    /// Bitcoin has no finality, so the confirmation depth *is* the safety
    /// policy: zero would commit the tip itself, and an unbounded one would
    /// hold an unbounded working set and serve an unboundedly stale balance.
    #[test]
    fn construction_bounds_the_confirmation_depth_without_rpc() {
        let limit = IndexerParams::<(), ()>::DEPTH_LIMIT;
        assert_eq!(IndexerParams::<(), ()>::CONFIRMED_DEPTH, 6);
        for depth in [0, limit + 1] {
            let script = Script::node(Vec::new());
            let error = indexer(IndexerParams {
                confirmed_depth: depth,
                ..IndexerParams::new(script.client(), ())
            })
            .err()
            .unwrap();
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert_eq!(
                error.message,
                format!("Bitcoin confirmation depth must be between 1 and {limit}")
            );
            assert!(script.requests().is_empty());
        }
        for depth in [1, 6, limit] {
            let script = Script::node(Vec::new());
            indexer(IndexerParams {
                confirmed_depth: depth,
                ..IndexerParams::new(script.client(), ())
            })
            .expect("a depth inside the accepted range");
        }
    }
}
