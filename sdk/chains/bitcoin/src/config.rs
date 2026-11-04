//! [`Config`]: one Bitcoin network's settings, and the pieces built from them.

use std::{sync::Arc, time::Duration};

use ::rpc::{BoxFuture, CallError, Client as _, Cluster, Endpoint, Http, Policy, Request};
use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, AssetKind, Cache, Future, Indexer, InputStore, Service, Storage,
    WalletFactoryBroadcaster,
};
use serde::Deserialize;
use serde_json::json;

use crate::{AddressType, Chain, Factory};
use crate::{BTC_DECIMALS, BTC_NAME, BTC_TICKER, Network};
use crate::{FeeRate, fee::Fees, rpc::Client};

/// Everything one Bitcoin network needs; built in code with [`Config::new`]
/// or from a [`base::NetworkConfig`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The network.
    pub network: Network,
    /// Its Bitcoin Core nodes: reads go to one, submissions to all.
    pub endpoints: Vec<Endpoint>,
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
    /// How long a broadcast transaction counts as pending before it is
    /// given up on, in milliseconds; default 259_200_000 (3 days).
    pub pending_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 30,000.
    pub interval_ms: u64,
    /// Fractional digits a view shows of the native currency; none: all.
    pub display_decimals: Option<u32>,
    /// How old a watch's start may be: about what history the nodes keep;
    /// none: no limit.
    pub max_history: Option<Duration>,
}

/// The knobs a [`base::NetworkConfig`]'s `settings` may set:
/// `address_type`, `fee_target_blocks`, `fee_floor`, `fee_ttl_ms`,
/// `pending_ttl_ms`, `interval_ms`; an absent one keeps its default.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    address_type: AddressType,
    fee_target_blocks: u16,
    fee_floor: u64,
    fee_ttl_ms: u64,
    pending_ttl_ms: u64,
    interval_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            address_type: AddressType::default(),
            fee_target_blocks: 6,
            fee_floor: 1_000,
            fee_ttl_ms: 60_000,
            pending_ttl_ms: 259_200_000,
            interval_ms: 30_000,
        }
    }
}

impl Config {
    /// The defaults around the two required values.
    #[must_use]
    pub fn new(network: Network, endpoints: Vec<Endpoint>) -> Self {
        Self::with(network, endpoints, Settings::default())
    }

    fn with(network: Network, endpoints: Vec<Endpoint>, settings: Settings) -> Self {
        Self {
            network,
            endpoints,
            confirmation_depth: 6,
            address_type: settings.address_type,
            fee_target_blocks: settings.fee_target_blocks,
            fee_floor: settings.fee_floor,
            fee_ttl_ms: settings.fee_ttl_ms,
            pending_ttl_ms: settings.pending_ttl_ms,
            interval_ms: settings.interval_ms,
            max_history: None,
            display_decimals: None,
        }
    }

    /// Rejects the value on its own, without I/O.
    ///
    /// # Errors
    ///
    /// For invalid endpoints, a depth outside `1..=1000`, a zero fee
    /// target, fee floor or interval.
    pub fn validate(&self) -> anyhow::Result<()> {
        for endpoint in &self.endpoints {
            endpoint.validate()?;
        }
        ensure!(
            (1..=1_000).contains(&self.confirmation_depth),
            "Bitcoin confirmation depth must be between 1 and 1000"
        );
        ensure!(
            self.fee_target_blocks > 0,
            "Bitcoin fee target must be positive"
        );
        ensure!(self.fee_floor > 0, "Bitcoin fee floor must be positive");
        ensure!(self.interval_ms > 0, "Bitcoin interval must be positive");
        Ok(())
    }

    /// Validates, connects to every endpoint and verifies the node.
    ///
    /// # Errors
    ///
    /// [`Self::validate`]'s refusals, or a node of another network.
    pub async fn connect(&self) -> anyhow::Result<Client> {
        self.open(self.cluster().await?).await
    }

    /// A cluster of the configured endpoints.
    async fn cluster(&self) -> anyhow::Result<Arc<dyn ::rpc::Client>> {
        ensure!(!self.endpoints.is_empty(), "Bitcoin needs an endpoint");
        let nodes = self
            .endpoints
            .iter()
            .map(Endpoint::connect)
            .collect::<Result<Vec<_>, _>>()?;
        let cluster = Cluster::new(nodes, height, Policy::default())
            .await
            .context("Bitcoin endpoints")?;
        Ok(Arc::new(cluster))
    }

    /// Validates, and a verified client over `transport`.
    async fn open(&self, transport: Arc<dyn ::rpc::Client>) -> anyhow::Result<Client> {
        self.validate()?;
        let client = Client::new(transport, self.network);
        client.verify().await?;
        Ok(client)
    }

    /// The chain read through `rpc`.
    ///
    /// # Errors
    ///
    /// For an invalid confirmation depth.
    pub fn chain(&self, rpc: &Client) -> anyhow::Result<Chain> {
        Chain::new(rpc.clone(), self.confirmation_depth)
    }

    /// The wallet factory: coins from `inputs`, fee rates from `rpc`,
    /// cached by its one estimator.
    #[must_use]
    pub fn factory(&self, rpc: &Client, inputs: Arc<dyn InputStore>) -> Factory {
        let fees = Fees {
            network: self.network.into(),
            rpc: rpc.clone(),
            inputs: Arc::clone(&inputs),
            target_blocks: self.fee_target_blocks,
            floor: FeeRate::from_sat_per_kvb(self.fee_floor),
            rate: Cache::new(Duration::from_millis(self.fee_ttl_ms)),
        };
        Factory {
            rpc: rpc.clone(),
            pending_ttl: Duration::from_millis(self.pending_ttl_ms),
            inputs,
            address_type: self.address_type,
            fees: Arc::new(fees),
        }
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

impl TryFrom<&base::NetworkConfig> for Config {
    type Error = anyhow::Error;

    /// Its network (by name), endpoints and native display, its
    /// `confirmation_depth` (`confirmations + 1`), and its knobs from
    /// `settings`.
    fn try_from(config: &base::NetworkConfig) -> anyhow::Result<Self> {
        let settings = Settings::deserialize(&config.settings).context("Bitcoin settings")?;
        let network =
            Network::deserialize(json!(config.network.name)).context("Bitcoin network")?;
        let mut built = Self::with(network, config.endpoints.clone(), settings);
        built.confirmation_depth = config.confirmations.saturating_add(1);
        built.max_history = config.max_history;
        for asset in &config.assets {
            ensure!(asset.kind == AssetKind::Native, "Bitcoin has no tokens");
            ensure!(asset.decimals == BTC_DECIMALS, "BTC has 8 decimals");
            built.display_decimals = asset.display_decimals;
        }
        ensure!(
            base::Config::network(&built) == config.network,
            "not a Bitcoin network"
        );
        Ok(built)
    }
}

impl base::Config for Config {
    fn network(&self) -> base::Network {
        self.network.into()
    }

    fn client(&self) -> Future<'_, anyhow::Result<Arc<dyn ::rpc::Client>>> {
        Box::pin(self.cluster())
    }

    fn build<'a>(
        &'a self,
        storage: &'a Storage,
        client: Arc<dyn ::rpc::Client>,
    ) -> Future<'a, anyhow::Result<Service>> {
        Box::pin(async move {
            let rpc = self.open(Arc::clone(&client)).await?;
            let chain = Arc::new(self.chain(&rpc)?);
            let indexer = Arc::new(Indexer::new(
                chain.clone(),
                Arc::clone(&storage.blocks),
                Arc::clone(&storage.indexes),
                Duration::from_millis(self.interval_ms),
            ));
            Ok(Service {
                indexer,
                wallets: WalletFactoryBroadcaster {
                    network: self.network.into(),
                    factory: Arc::new(self.factory(&rpc, Arc::clone(&storage.inputs))),
                    broadcaster: Arc::new(rpc),
                },
                assets: std::iter::once(self.asset()).collect(),
                storage: storage.clone(),
                client,
                dated: Some(chain),
                max_history: self.max_history,
                confirmations: self.confirmation_depth.saturating_sub(1),
            })
        })
    }
}

/// A node's tip, which picks the cluster's primary.
fn height(node: &Http) -> BoxFuture<'_, Result<u64, CallError>> {
    Box::pin(async move {
        let reply = node
            .request(Request::new("getblockcount", json!([])))
            .await??;
        Ok(reply.deserialize()?)
    })
}
