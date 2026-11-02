//! [`Config`]: one Bitcoin network's settings, and the pieces built from them.

use std::{sync::Arc, time::Duration};

use ::rpc::{BoxFuture, CallError, Client as _, Cluster, Endpoint, Http, Policy, Request};
use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, Cache, Future, Indexer, InputStore, Service, Storage, Timer,
    WalletFactoryBroadcaster,
};
use serde::Deserialize;
use serde_json::json;

use crate::{AddressType, Chain, Factory};
use crate::{BTC_DECIMALS, BTC_NAME, BTC_TICKER, Network};
use crate::{FeeRate, fee::Fees, rpc::Client};

/// Everything one Bitcoin network needs.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The network.
    pub network: Network,
    /// Its Bitcoin Core nodes: reads go to one, submissions to all.
    pub endpoints: Vec<Endpoint>,
    /// Blocks a block needs on top of it, itself included, to be final;
    /// 1 to 1000, default 6. Bitcoin has no finality of its own.
    #[serde(default = "Config::default_confirmation_depth")]
    pub confirmation_depth: u64,
    /// P2WPKH (default) or `taproot`.
    #[serde(default)]
    pub address_type: AddressType,
    /// Confirmation target of the node's fee estimate, in blocks; default 6.
    #[serde(default = "Config::default_fee_target_blocks")]
    pub fee_target_blocks: u16,
    /// The lowest fee rate sent, in satoshis per 1,000 virtual bytes; also
    /// the rate when the node cannot estimate. Default 1,000 (Core's minimum
    /// relay rate).
    #[serde(default = "Config::default_fee_floor")]
    pub fee_floor: u64,
    /// How long a fetched fee rate prices sends, in milliseconds; default
    /// 60,000.
    #[serde(default = "Config::default_fee_ttl_ms")]
    pub fee_ttl_ms: u64,
    /// How long a broadcast transaction counts as pending before it is
    /// given up on, in milliseconds; default 259_200_000 (3 days).
    #[serde(default = "Config::default_pending_ttl_ms")]
    pub pending_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 30,000.
    #[serde(default = "Config::default_interval_ms")]
    pub interval_ms: u64,
}

impl Config {
    /// The defaults around the two required values.
    #[must_use]
    pub fn new(network: Network, endpoints: Vec<Endpoint>) -> Self {
        Self {
            network,
            endpoints,
            confirmation_depth: Self::default_confirmation_depth(),
            address_type: AddressType::default(),
            fee_target_blocks: Self::default_fee_target_blocks(),
            fee_floor: Self::default_fee_floor(),
            fee_ttl_ms: Self::default_fee_ttl_ms(),
            pending_ttl_ms: Self::default_pending_ttl_ms(),
            interval_ms: Self::default_interval_ms(),
        }
    }

    const fn default_interval_ms() -> u64 {
        30_000
    }

    const fn default_confirmation_depth() -> u64 {
        6
    }

    const fn default_fee_target_blocks() -> u16 {
        6
    }

    const fn default_fee_ttl_ms() -> u64 {
        60_000
    }

    const fn default_pending_ttl_ms() -> u64 {
        259_200_000
    }

    const fn default_fee_floor() -> u64 {
        1_000
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
        }
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
            let chain = self.chain(&rpc)?;
            let indexer = Arc::new(Indexer::new(
                Arc::new(chain),
                Arc::clone(&storage.blocks),
                Arc::clone(&storage.indexes),
                Arc::new(Timer(Duration::from_millis(self.interval_ms))),
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
