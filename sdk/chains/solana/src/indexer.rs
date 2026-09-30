//! The Solana indexer: [`indexer`] over [`IndexerParams`].

use indexing::{IndexError, Indexer, SyncConfig, SyncPolicy};

use crate::{BlockInterpreter, RpcClient, Source};

/// Everything one Solana indexer needs. Fields are public; [`Self::new`]
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
    /// Solana carries no confirmation number at all: the source reads only
    /// finalized (rooted) blocks, so its tip is already the confirmed line and
    /// nothing is ever held above it.
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

/// Builds the indexer `params` describes, for the client's own cluster. No
/// I/O: the client was verified when it connected.
///
/// `indexing::Indexer` itself, over this chain's block source and interpreter,
/// rather than a newtype that forwarded every capability to one: it implements
/// `indexing::{Indexing, Checkpoint, Pending}` already. It answers no query:
/// a history page is its store's, joined to the blocks this index holds above
/// the line by `wallets::PendingJoin`. Store it as
/// `Arc<dyn indexing::Indexing>` or combine several in an `indexing::Index`.
///
/// The commit announcements this chain's submission recovery waits on are not
/// allocated here. They are one `indexing::Listener` that
/// [`crate::WalletFactory::new`] registers on the indexer it is handed, so an
/// indexing-only caller pays for no channel at all
/// (`crate::sender::CommitSignal`).
///
/// A function rather than an inherent constructor, because this crate does
/// not own [`Indexer`]; [`IndexerParams`] stays what it is, the argument,
/// rather than becoming a factory for its own product.
///
/// # Errors
///
/// `InvalidRequest` for an invalid [`SyncPolicy`].
pub fn indexer<C, R>(
    params: IndexerParams<C, R>,
) -> Result<Indexer<Source<C>, BlockInterpreter, R>, IndexError> {
    let network = params.rpc.network().clone();
    Ok(Indexer::new(
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
        let script = Script::exchanges([]);
        let index = indexer(IndexerParams::new(script.client(), ())).unwrap();
        assert_eq!(index.network(), &Script::network().canonical());
        script.exhausted();
    }

    #[test]
    fn construction_rejects_a_zero_batch_without_rpc() {
        let script = Script::exchanges([]);
        let error = indexer(IndexerParams {
            sync: SyncPolicy::new(0),
            ..IndexerParams::new(script.client(), ())
        })
        .err()
        .unwrap();
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        script.exhausted();
    }
}
