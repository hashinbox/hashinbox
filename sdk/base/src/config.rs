//! [`Config`]: a chain's configuration of one network, built into a
//! [`Service`] over the application's [`Storage`]; and [`NetworkConfig`],
//! the chain-neutral description each chain builds its `Config` from.

use std::{fmt, sync::Arc, time::Duration};

use serde_json::Value;

use crate::{
    Assets, BlockStore, Dated, Future, Holdings, Indexer, InputStore, Network,
    WalletFactoryBroadcaster, WalletIndexStore, WalletStore,
};

/// One network as an application stores it, whatever its chain: the facts
/// every chain has, its assets and endpoints, and `settings`, the chain's
/// own knobs. Each chain crate builds its `Config` from it
/// (`TryFrom<&NetworkConfig>`), reading every absent knob as its default.
#[derive(Clone, Debug)]
pub struct NetworkConfig {
    /// The chain and network name.
    pub network: Network,
    /// The chain's own identity of the network (Ethereum chain id, Solana
    /// genesis hash); `None` where the name is enough.
    pub chain_id: Option<String>,
    /// Whether its coins are worthless test coins.
    pub testnet: bool,
    /// The chain's block (or slot) time.
    pub block_time: Duration,
    /// Positions a block needs below the tip to count as confirmed.
    pub confirmations: u64,
    /// How old a watch's start may be; `None`: no limit.
    pub max_history: Option<Duration>,
    /// An address's explorer page, `{address}` filled in; `None`: none.
    pub explorer_address: Option<String>,
    /// A transaction's explorer page, `{tx}` filled in; `None`: none.
    pub explorer_transaction: Option<String>,
    /// The chain's own knobs (fee limits, poll interval, ...): a JSON object.
    pub settings: Value,
    /// The native currency and every token.
    pub assets: Vec<AssetConfig>,
    /// The nodes, in failover order.
    pub endpoints: Vec<rpc::Endpoint>,
}

/// One asset of a [`NetworkConfig`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    /// The asset id: `native`, or the token's contract (mint).
    pub asset: String,
    /// The network's own currency, or a token.
    pub kind: AssetKind,
    /// The token's contract (mint); `None` for the native currency.
    pub contract: Option<String>,
    /// Display symbol, such as `ETH` or `USDC`.
    pub ticker: String,
    /// Fractional digits between the display unit and the atomic unit.
    pub decimals: u32,
    /// Fractional digits a view shows; `None`: all.
    pub display_decimals: Option<u32>,
}

/// What an [`AssetConfig`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    /// The network's own currency.
    Native,
    /// A token, by its contract (mint).
    Contract,
}

/// Every store the services need, supplied by the application.
#[derive(Clone)]
pub struct Storage {
    /// Kept blocks, which the indexers write.
    pub blocks: Arc<dyn BlockStore>,
    /// How far each watched asset of each address is indexed.
    pub indexes: Arc<dyn WalletIndexStore>,
    /// Unspent inputs, which UTXO wallets spend.
    pub inputs: Arc<dyn InputStore>,
    /// Keyed wallets.
    pub wallets: Arc<dyn WalletStore>,
    /// Who holds what, so fees are priced for their recipients.
    pub holdings: Arc<dyn Holdings>,
}

impl fmt::Debug for Storage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Storage").finish_non_exhaustive()
    }
}

/// One network, ready to run.
pub struct Service {
    /// Indexes the network into [`Storage::blocks`]; shared, so it can
    /// step ([`Indexer::next`]) while still answering watches and stats.
    pub indexer: Arc<Indexer>,
    /// Makes the network's wallets and broadcasts their transactions.
    pub wallets: WalletFactoryBroadcaster,
    /// The native currency and every configured token.
    pub assets: Assets,
    /// The stores it was built over.
    pub storage: Storage,
    /// The node transport every part of the service reads and broadcasts
    /// through: the one passed to [`Config::build`].
    pub client: Arc<dyn rpc::Client>,
    /// Finds the network's blocks by time; `None` where the chain cannot.
    pub dated: Option<Arc<dyn Dated>>,
    /// How old a watch's start may be: about what history the nodes keep;
    /// `None`: no limit.
    pub max_history: Option<Duration>,
    /// Positions a block needs below the tip to count as confirmed: the
    /// confirmed position is `tip - confirmations`.
    pub confirmations: u64,
}

impl fmt::Debug for Service {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Service")
            .field("network", &self.wallets.network)
            .finish_non_exhaustive()
    }
}

/// What a node says about itself: which chain it serves and its tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// The chain it serves, as the network's `chain_id` writes it (EVM chain
    /// ID, Solana genesis hash, Bitcoin Core chain name).
    pub chain_id: String,
    /// Its newest block (or slot).
    pub tip: u64,
}

/// Reads one node's tip in the chain's own unit (blocks, slots).
pub type Height = for<'a> fn(&'a rpc::Http) -> rpc::BoxFuture<'a, Result<u64, rpc::CallError>>;

/// A chain's configuration of one network.
pub trait Config: Send + Sync {
    /// The network it configures.
    fn network(&self) -> Network;

    /// How a node's tip is read: what a cluster of the network's nodes
    /// ranks them by.
    fn height(&self) -> Height;

    /// What the node behind `client` serves; asked once, when a node is
    /// added, never while running.
    ///
    /// # Errors
    ///
    /// A node failure, or a node of another chain.
    fn info(&self, client: Arc<dyn rpc::Client>) -> Future<'_, anyhow::Result<NodeInfo>>;

    /// Validates and builds the service over `storage`, every part of it reaching the
    /// nodes through `client` (typically a [`rpc::Swap`] the caller
    /// keeps to change nodes live).
    ///
    /// # Errors
    ///
    /// An invalid configuration.
    fn build<'a>(
        &'a self,
        storage: &'a Storage,
        client: Arc<dyn rpc::Client>,
    ) -> Future<'a, anyhow::Result<Service>>;
}
