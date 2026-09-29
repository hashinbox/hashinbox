//! The Ethereum indexer: [`indexer`] over [`IndexerParams`].

use indexing::{IndexError, Service, SyncConfig, SyncPolicy};

use crate::{BlockInterpreter, RpcClient, Source};

/// One Ethereum network's indexer: source, interpreter, and repository.
///
/// `indexing::Service` itself, over this chain's block source and interpreter,
/// rather than a newtype that forwarded every capability to one: it implements
/// `indexing::{Indexing, Checkpoint}` already, and hands out the history reader
/// over its own store (`Service::history`). Store it as
/// `Arc<dyn indexing::Indexing>` or combine several in an `indexing::Index`.
pub type Indexer<C, R> = Service<Source<C>, BlockInterpreter, R>;

/// Everything one Ethereum indexer needs. Fields are public; [`Self::new`]
/// fills the chain defaults.
pub struct IndexerParams<C, R> {
    /// Verified client. Clone it to share with [`crate::WalletFactory`].
    pub rpc: RpcClient<C>,
    /// The one store this index registers observations in and commits to. It
    /// serves every network, so the same value is handed to every chain.
    pub repository: R,
    /// Defaults to [`IndexerParams::SYNC`].
    pub sync: SyncPolicy,
}

impl<C, R> IndexerParams<C, R> {
    /// Batches of a hundred blocks.
    ///
    /// Ethereum carries no confirmation number at all: the node states where
    /// its own `finalized` block is, and the indexer commits there.
    pub const SYNC: SyncPolicy = SyncPolicy::new(100);

    /// Chain-default synchronization.
    #[must_use]
    pub const fn new(rpc: RpcClient<C>, repository: R) -> Self {
        Self {
            rpc,
            repository,
            sync: Self::SYNC,
        }
    }
}

/// Builds the indexer `params` describes, for the client's own network. No
/// I/O: the client was verified when it connected.
///
/// A function rather than an inherent constructor, because [`Indexer`] is an
/// alias for `indexing::Service` and this crate does not own that type;
/// [`IndexerParams`] stays what it is, the argument, rather than becoming a
/// factory for its own product.
///
/// # Errors
///
/// `InvalidRequest` for an invalid [`SyncPolicy`].
pub fn indexer<C, R>(params: IndexerParams<C, R>) -> Result<Indexer<C, R>, IndexError> {
    let network = params.rpc.network().clone();
    Ok(Service::new(
        Source::new(params.rpc),
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
        assert_eq!(error.message, "batch size must be greater than zero");
        assert!(script.requests().is_empty());
    }
}
