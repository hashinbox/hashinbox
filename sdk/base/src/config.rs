//! [`NetworkConfig`]: one network, whatever its chain; [`ServiceFactory`]:
//! a chain building it into a [`Service`] over the application's
//! [`Storage`].

use std::{fmt, sync::Arc, time::Duration};

use serde_json::Value;

use crate::{
    Assets, Balances, BlockStore, Chain, Confirmer, Dated, FeeRateProvider, Holdings, Indexer,
    InputStore, Network, TransactionIndexStore, WalletFactoryBroadcaster, WalletIndexStore,
    WalletStore,
};

/// One network as an application stores it, whatever its chain: the facts
/// every chain has, its assets, and `settings`, the chain's own knobs. Its
/// nodes are not here: they belong to its [`crate::Transport`]. Each chain crate builds its `Config` from it
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
    /// Positions below the tip a watch's start may be; `None`: no limit.
    /// At least `confirmations`. A chain that reads balances has none: its
    /// line is its final block.
    pub max_depth: Option<u64>,
    /// An address's explorer page, `{address}` filled in; `None`: none.
    pub explorer_address: Option<String>,
    /// A transaction's explorer page, `{tx}` filled in; `None`: none.
    pub explorer_transaction: Option<String>,
    /// The chain's own knobs (fee limits, poll interval, ...): a JSON object.
    pub settings: Value,
    /// The native currency and every token.
    pub assets: Vec<AssetConfig>,
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
    /// How far the blocks after each sent transaction are read for it.
    pub transaction_indexes: Arc<dyn TransactionIndexStore>,
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
    /// step ([`Indexer::next`]) while still answering stats.
    pub indexer: Arc<Indexer>,
    /// Makes the network's wallets and broadcasts their transactions.
    pub wallets: WalletFactoryBroadcaster,
    /// The native currency and every configured token.
    pub assets: Assets,
    /// The stores it was built over.
    pub storage: Storage,
    /// The node transport every part of the service reads and broadcasts
    /// through: the one passed to [`ServiceFactory::service`].
    pub client: Arc<dyn rpc::Client>,
    /// Finds the network's blocks by time; `None` where the chain cannot.
    pub dated: Option<Arc<dyn Dated>>,
    /// Reads a balance at a final block; `None` where the chain cannot.
    pub balances: Option<Arc<dyn Balances>>,
    /// Tells whether a transaction was mined through a final block; `None`
    /// where the chain cannot.
    pub confirmer: Option<Arc<dyn Confirmer>>,
    /// Positions a block needs below the tip to count as confirmed: the
    /// confirmed position is `tip - confirmations`.
    pub confirmations: u64,
    /// How its sent payments are checked and bumped.
    pub tracking: Tracking,
    /// The network's fee rate, cached: the one every builder and the
    /// estimator of [`Self::wallets`] price with.
    pub rates: Arc<dyn FeeRateProvider>,
}

/// How a chain's sent payments are checked and bumped (design 084), from
/// its config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tracking {
    /// How often a broadcast job checks its payment.
    pub check: Duration,
    /// Blocks its transaction index reads past `broadcasted_at`, the
    /// payment in none of them, before a bump; unused for a transaction
    /// that expires (bumped once dead).
    pub bump_after: u64,
    /// Bumps per payment at most; 0: never bumped.
    pub max_bumps: u32,
}

impl Default for Tracking {
    /// Checked every minute, never bumped.
    fn default() -> Self {
        Self {
            check: Duration::from_secs(60),
            bump_after: 0,
            max_bumps: 0,
        }
    }
}

impl fmt::Debug for Service {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Service")
            .field("network", &self.wallets.network)
            .finish_non_exhaustive()
    }
}

/// One chain: a network's config, read once, then built into its
/// [`Service`] over the application's stores and the one client every part
/// reaches its nodes through.
pub trait ServiceFactory: Sized {
    /// The chain it builds.
    const CHAIN: Chain;

    /// The chain's settings from a stored `config`, checked once.
    ///
    /// # Errors
    ///
    /// Settings that do not decode, or a network of another chain.
    fn new(config: &NetworkConfig) -> anyhow::Result<Self>;

    /// Its service over `storage` and `client`; asks no node.
    ///
    /// # Errors
    ///
    /// An invalid configuration.
    fn service(&self, storage: &Storage, client: Arc<dyn rpc::Client>) -> anyhow::Result<Service>;
}
