//! [`Config`]: one Bitcoin network's settings, and the pieces built from them.

use std::sync::Arc;

use anyhow::{Context as _, ensure};
use base::{Asset, AssetInfo, InputStore};
use json_rpc::{BoxFuture, CallError, Client as _, Cluster, Endpoint, Http, Policy, Request};
use serde::Deserialize;
use serde_json::json;

use crate::{AddressType, Chain, Factory};
use crate::{BTC_DECIMALS, BTC_NAME, BTC_TICKER, Network};
use crate::{FeeRate, rpc::Client};

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
        }
    }

    const fn default_confirmation_depth() -> u64 {
        6
    }

    const fn default_fee_target_blocks() -> u16 {
        6
    }

    const fn default_fee_floor() -> u64 {
        1_000
    }

    /// Rejects the value on its own, without I/O.
    ///
    /// # Errors
    ///
    /// For no or invalid endpoints, a depth outside `1..=1000`, a zero fee
    /// target or a zero fee floor.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.endpoints.is_empty(), "Bitcoin needs an endpoint");
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
        Ok(())
    }

    /// Validates, connects to every endpoint and verifies the node.
    ///
    /// # Errors
    ///
    /// [`Self::validate`]'s refusals, or a node of another network.
    pub async fn connect(&self) -> anyhow::Result<Client> {
        self.validate()?;
        let nodes = self
            .endpoints
            .iter()
            .map(Endpoint::connect)
            .collect::<Result<Vec<_>, _>>()?;
        let cluster = Cluster::new(nodes, height, Policy::default())
            .await
            .context("Bitcoin endpoints")?;
        let client = Client::new(Arc::new(cluster), self.network);
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

    /// The wallet factory: coins from `inputs`, fees from `rpc`.
    #[must_use]
    pub fn factory(&self, rpc: &Client, inputs: Arc<dyn InputStore>) -> Factory {
        Factory {
            rpc: rpc.clone(),
            inputs,
            address_type: self.address_type,
            fee_target_blocks: self.fee_target_blocks,
            fee_floor: FeeRate::from_sat_per_kvb(self.fee_floor),
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

/// A node's tip, which picks the cluster's primary.
fn height(node: &Http) -> BoxFuture<'_, Result<u64, CallError>> {
    Box::pin(async move {
        let reply = node
            .request(Request::new("getblockcount", json!([])))
            .await??;
        Ok(reply.deserialize()?)
    })
}
