//! The Ethereum indexer: [`indexer`] over [`IndexerParams`].

use std::time::Duration;

use indexing::{Budget, Config, IndexError, Indexer, Schedule};

use crate::{BlockInterpreter, RpcClient, Source};

/// Everything one Ethereum indexer needs. Fields are public; [`Self::new`]
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
}

impl<C, R> IndexerParams<C, R> {
    /// Batches of a hundred blocks.
    ///
    /// Ethereum carries no confirmation number at all: the node states where
    /// its own `finalized` block is, and the indexer commits there.
    pub const SYNC: Budget = Budget::new(100);

    /// Half of Ethereum's twelve-second slot.
    pub const POLL: Schedule = Schedule::every(Duration::from_secs(6));

    /// Chain-default synchronization.
    #[must_use]
    pub const fn new(rpc: RpcClient<C>, repository: R) -> Self {
        Self {
            rpc,
            repository,
            sync: Self::SYNC,
            schedule: Self::POLL,
        }
    }
}

/// Builds the indexer `params` describes, for the client's own network. No
/// I/O: the client was verified when it connected.
///
/// `indexing::Indexer` itself, over this chain's block source and interpreter,
/// rather than a newtype that forwarded every capability to one: it implements
/// `indexing::{Indexing, Checkpoint, Pending}` already. It answers no query:
/// reading history is the application's, completed by the blocks this index
/// holds above the line (`indexing::Pending`). Store it as
/// `Arc<dyn indexing::Indexing>` or combine several in an `indexing::Index`.
///
/// A function rather than an inherent constructor, because this crate does
/// not own [`Indexer`]; [`IndexerParams`] stays what it is, the argument,
/// rather than becoming a factory for its own product.
///
/// # Errors
///
/// `InvalidRequest` for an invalid [`Budget`].
pub fn indexer<C, R>(
    params: IndexerParams<C, R>,
) -> Result<Indexer<Source<C>, BlockInterpreter, R>, IndexError> {
    let network = params.rpc.network().clone();
    Ok(Indexer::new(
        Source::new(params.rpc),
        BlockInterpreter::new(&network)?,
        params.repository,
        Config::new(&network, params.sync, params.schedule)?,
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
            sync: Budget::new(0),
            ..IndexerParams::new(script.client(), ())
        })
        .err()
        .unwrap();
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        assert_eq!(error.message, "batch size must be greater than zero");
        assert!(script.requests().is_empty());
    }
}
