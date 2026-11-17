//! [`Config`]: one Bitcoin network's settings; [`Factory`]: the pieces
//! built from them, a stored network's service among them.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, AssetKind, Indexer, InputStore, NetworkConfig, Service, Storage,
    TransactionIndexer, WalletFactoryBroadcaster, WalletIndexer,
};
use serde::Deserialize;
use serde_json::json;

use crate::{AddressType, CHAIN_NAME, Chain, WalletFactory};
use crate::{BTC_DECIMALS, BTC_NAME, BTC_TICKER, Network};
use crate::{FeeRate, fee::Estimator, fee_cache::Cached, rpc::Client};

/// Everything one Bitcoin network needs; built in code with [`Config::new`]
/// or from a [`base::NetworkConfig`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The network.
    pub network: Network,
    /// Blocks a block needs on top of it, itself included, to be final;
    /// 1 to 1000, default 6. Bitcoin has no finality of its own. Also the
    /// service's `confirmations`: `confirmation_depth - 1`.
    pub confirmation_depth: u64,
    /// P2WPKH (default) or `taproot`.
    pub address_type: AddressType,
    /// Confirmation target of the node's fee estimate, in blocks; default 6.
    pub fee_target_blocks: u16,
    /// The lowest fee rate sent, in satoshis per 1,000 virtual bytes; also
    /// the rate when the node cannot estimate. Default 1,000 (Core's minimum
    /// relay rate).
    pub fee_floor: u64,
    /// How long a fetched fee rate prices sends, in milliseconds; default
    /// 60,000.
    pub fee_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 30,000.
    pub interval_ms: u64,
    /// Blocks the indexer fetches per step; default 100
    /// ([`base::DEFAULT_BATCH`]). Lower it for a node that limits requests.
    pub batch: u64,
    /// Fractional digits a view shows of the native currency; none: all.
    pub display_decimals: Option<u32>,
    /// Positions below the tip a watch's start may be; none: no limit. At
    /// least `confirmation_depth - 1`.
    pub max_depth: Option<u64>,
}

/// The knobs a [`base::NetworkConfig`]'s `settings` may set:
/// `address_type`, `fee_target_blocks`, `fee_floor`, `fee_ttl_ms`,
/// `interval_ms`, `batch`; an absent one keeps its default.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    address_type: AddressType,
    fee_target_blocks: u16,
    fee_floor: u64,
    fee_ttl_ms: u64,
    interval_ms: u64,
    batch: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            address_type: AddressType::default(),
            fee_target_blocks: 6,
            fee_floor: 1_000,
            fee_ttl_ms: 60_000,
            interval_ms: 30_000,
            batch: base::DEFAULT_BATCH,
        }
    }
}

impl Config {
    /// The defaults around the two required values.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self::with(network, Settings::default())
    }

    fn with(network: Network, settings: Settings) -> Self {
        Self {
            network,
            confirmation_depth: 6,
            address_type: settings.address_type,
            fee_target_blocks: settings.fee_target_blocks,
            fee_floor: settings.fee_floor,
            fee_ttl_ms: settings.fee_ttl_ms,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            max_depth: None,
            display_decimals: None,
        }
    }

    /// Rejects the value on its own, without I/O.
    ///
    /// # Errors
    ///
    /// For a depth outside `1..=1000`, a depth limit below
    /// `confirmation_depth - 1`, a zero fee target, fee floor or interval.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            (1..=1_000).contains(&self.confirmation_depth),
            "Bitcoin confirmation depth must be between 1 and 1000"
        );
        let confirmations = self.confirmation_depth - 1;
        ensure!(
            self.max_depth.is_none_or(|limit| limit >= confirmations),
            "Bitcoin max_depth must be at least its confirmations ({confirmations})"
        );
        ensure!(
            self.fee_target_blocks > 0,
            "Bitcoin fee target must be positive"
        );
        ensure!(self.fee_floor > 0, "Bitcoin fee floor must be positive");
        ensure!(self.interval_ms > 0, "Bitcoin interval must be positive");
        Ok(())
    }

    /// BTC on this network.
    #[must_use]
    pub fn asset(&self) -> AssetInfo {
        AssetInfo {
            asset: Asset::native(self.network.into()),
            decimals: BTC_DECIMALS,
            name: Some(BTC_NAME.to_owned()),
            ticker: Some(BTC_TICKER.to_owned()),
            display_decimals: self.display_decimals,
        }
    }
}

impl TryFrom<&NetworkConfig> for Config {
    type Error = anyhow::Error;

    /// Its network (by name) and native display, its
    /// `confirmation_depth` (`confirmations + 1`), and its knobs from
    /// `settings`.
    fn try_from(config: &NetworkConfig) -> anyhow::Result<Self> {
        let settings = Settings::deserialize(&config.settings).context("Bitcoin settings")?;
        let network =
            Network::deserialize(json!(config.network.name)).context("Bitcoin network")?;
        let mut built = Self::with(network, settings);
        built.confirmation_depth = config.confirmations.saturating_add(1);
        built.max_depth = config.max_depth;
        for asset in &config.assets {
            ensure!(asset.kind == AssetKind::Native, "Bitcoin has no tokens");
            ensure!(asset.decimals == BTC_DECIMALS, "BTC has 8 decimals");
            built.display_decimals = asset.display_decimals;
        }
        ensure!(
            base::Network::from(built.network) == config.network,
            "not a Bitcoin network"
        );
        Ok(built)
    }
}

/// Builds Bitcoin's pieces from a [`Config`]; as its
/// [`base::ServiceFactory`], a stored network's whole service.
pub struct Factory {
    config: Config,
}

impl From<Config> for Factory {
    fn from(config: Config) -> Self {
        Self { config }
    }
}

impl Factory {
    /// The block reader over `rpc`.
    ///
    /// # Errors
    ///
    /// For an invalid confirmation depth.
    pub fn chain(&self, rpc: &Client) -> anyhow::Result<Chain> {
        Ok(Chain::new(rpc.clone(), self.config.confirmation_depth)?
            .with_max_depth(self.config.max_depth))
    }

    /// The wallet factory over `rpc`: coins from `inputs`; fee rates from
    /// `rpc`, reused by its one cached estimator for `fee_ttl_ms`.
    #[must_use]
    pub fn wallets(&self, rpc: &Client, inputs: Arc<dyn InputStore>) -> WalletFactory {
        let config = &self.config;
        let precise = Estimator {
            network: config.network.into(),
            rpc: rpc.clone(),
            inputs: Arc::clone(&inputs),
            target_blocks: config.fee_target_blocks,
            floor: FeeRate::from_sat_per_kvb(config.fee_floor),
        };
        let fees = Cached::new(precise, Duration::from_millis(config.fee_ttl_ms));
        WalletFactory {
            rpc: rpc.clone(),
            inputs,
            address_type: config.address_type,
            fees: Arc::new(fees),
        }
    }
}

impl base::ServiceFactory for Factory {
    const CHAIN: base::Chain = CHAIN_NAME;

    fn new(config: &NetworkConfig) -> anyhow::Result<Self> {
        Ok(Config::try_from(config)?.into())
    }

    /// Validates, and the service over `storage`, every part of it reaching
    /// the nodes through `client`; asks no node.
    fn service(
        &self,
        storage: &Storage,
        client: Arc<dyn ::rpc::Client>,
    ) -> anyhow::Result<Service> {
        let config = &self.config;
        config.validate()?;
        let rpc = Client::new(Arc::clone(&client), config.network);
        let chain = Arc::new(self.chain(&rpc)?);
        let wallets = WalletIndexer::new(Arc::clone(&storage.indexes), chain.clone(), None);
        let transactions = TransactionIndexer::new(
            Arc::clone(&storage.transaction_indexes),
            chain.clone(),
            None,
        );
        let indexer = Arc::new(
            Indexer::new(
                chain.clone(),
                Arc::clone(&storage.blocks),
                vec![Arc::new(wallets), Arc::new(transactions)],
                Duration::from_millis(config.interval_ms),
            )
            .with_batch(config.batch),
        );
        Ok(Service {
            indexer,
            wallets: WalletFactoryBroadcaster {
                network: config.network.into(),
                factory: Arc::new(self.wallets(&rpc, Arc::clone(&storage.inputs))),
                broadcaster: Arc::new(rpc),
            },
            assets: std::iter::once(config.asset()).collect(),
            storage: storage.clone(),
            client,
            dated: Some(chain),
            balances: None,
            confirmer: None,
            confirmations: config.confirmation_depth.saturating_sub(1),
        })
    }
}
