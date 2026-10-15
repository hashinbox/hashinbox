//! [`Config`]: a chain's configuration of one network, built into a
//! [`Service`] over the application's [`Storage`].

use std::{fmt, sync::Arc};

use crate::{
    Assets, BlockStore, Future, Indexer, InputStore, Network, TransactionStore,
    WalletFactoryBroadcaster, WalletIndexStore, WalletStore,
};

/// Every store the services need, supplied by the application.
#[derive(Clone)]
pub struct Storage {
    /// Kept blocks, which the indexers write.
    pub blocks: Arc<dyn BlockStore>,
    /// How far each watched asset of each address is indexed.
    pub indexes: Arc<dyn WalletIndexStore>,
    /// Unspent inputs, which UTXO wallets spend.
    pub inputs: Arc<dyn InputStore>,
    /// Kept transactions.
    pub transactions: Arc<dyn TransactionStore>,
    /// Keyed wallets.
    pub wallets: Arc<dyn WalletStore>,
}

impl fmt::Debug for Storage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Storage").finish_non_exhaustive()
    }
}

/// One network, ready to run.
pub struct Service {
    /// Indexes the network into [`Storage::blocks`]; shared, so it can
    /// [`Indexer::run`] while still answering watches and stats.
    pub indexer: Arc<Indexer>,
    /// Makes the network's wallets and broadcasts their transactions.
    pub wallets: WalletFactoryBroadcaster,
    /// The native currency and every configured token.
    pub assets: Assets,
    /// The stores it was built over.
    pub storage: Storage,
    /// The node transport every part of the service reads and broadcasts
    /// through: the one passed to [`Config::build`].
    pub client: Arc<dyn json_rpc::Client>,
}

impl fmt::Debug for Service {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Service")
            .field("network", &self.wallets.network)
            .finish_non_exhaustive()
    }
}

/// A chain's configuration of one network.
pub trait Config: Send + Sync {
    /// The network it configures.
    fn network(&self) -> Network;

    /// The chain's own transport: a cluster over its configured endpoints.
    ///
    /// # Errors
    ///
    /// No or invalid endpoints, or none reachable.
    fn client(&self) -> Future<'_, anyhow::Result<Arc<dyn json_rpc::Client>>>;

    /// Validates, verifies once that `client` serves this network, and
    /// builds the service over `storage`, every part of it reaching the
    /// nodes through `client` (typically a [`json_rpc::Swap`] the caller
    /// keeps to change nodes live).
    ///
    /// # Errors
    ///
    /// An invalid configuration, or an unreachable or foreign node.
    fn build<'a>(
        &'a self,
        storage: &'a Storage,
        client: Arc<dyn json_rpc::Client>,
    ) -> Future<'a, anyhow::Result<Service>>;
}
