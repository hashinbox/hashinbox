//! [`Config`]: one Ethereum network's settings, and the pieces built from them.

use std::{sync::Arc, time::Duration};

use alloy_primitives::hex;
use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, Assets, Future, Indexer, Service, Storage, Timer, WalletFactoryBroadcaster,
};
use json_rpc::{Cluster, Endpoint, Policy, Retry};
use serde::{Deserialize, Deserializer, de};

use crate::{
    Chain, Client, ETH_DECIMALS, ETH_NAME, ETH_TICKER, Factory, FeeLimits, Network, Tokens,
    fee::Estimator, rpc,
};

/// Everything one Ethereum network needs; built in code with
/// [`Config::new`] or read from any `serde` format.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The stable application name of the network, such as `mainnet`.
    pub network: String,
    /// The EIP-155 chain ID the node is checked against.
    pub chain_id: u64,
    /// The nodes: reads go to one, submissions to all.
    pub endpoints: Vec<Endpoint>,
    /// Allowlisted ERC-20 tokens: the only tokens indexed and sent.
    #[serde(default)]
    pub tokens: Vec<Token>,
    /// Ceilings on the fees a transaction is built under.
    #[serde(default)]
    pub limits: FeeLimits,
    /// How long fetched fees price sends, in milliseconds; default 12,000.
    #[serde(default = "Config::default_fee_ttl_ms")]
    pub fee_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 12,000.
    #[serde(default = "Config::default_interval_ms")]
    pub interval_ms: u64,
}

/// One allowlisted ERC-20 token and the decimals its contract must report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    /// The contract, written as canonical lowercase text.
    #[serde(deserialize_with = "contract")]
    pub contract: alloy_primitives::Address,
    /// What its `decimals()` must report.
    pub decimals: u8,
}

impl Config {
    /// `network` over `endpoints`, with no tokens, default fee limits, and
    /// tracing on.
    #[must_use]
    pub fn new(network: Network, endpoints: Vec<Endpoint>) -> Self {
        Self {
            network: network.slug,
            chain_id: network.chain_id,
            endpoints,
            tokens: Vec::new(),
            limits: FeeLimits::default(),
            fee_ttl_ms: Self::default_fee_ttl_ms(),
            interval_ms: Self::default_interval_ms(),
        }
    }

    const fn default_fee_ttl_ms() -> u64 {
        12_000
    }

    const fn default_interval_ms() -> u64 {
        12_000
    }

    /// The configured network.
    #[must_use]
    pub fn native(&self) -> Network {
        Network::new(self.chain_id, self.network.as_str())
    }

    /// Rejects the value on its own, without I/O.
    ///
    /// # Errors
    ///
    /// For an invalid network, invalid endpoints, a zero token
    /// contract, or contradicting fee limits.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        for endpoint in &self.endpoints {
            endpoint.validate()?;
        }
        ensure!(
            self.tokens.iter().all(|token| !token.contract.is_zero()),
            "Ethereum token contract must not be the zero address"
        );
        ensure!(self.interval_ms > 0, "Ethereum interval must be positive");
        self.limits.validate()
    }

    /// Validates, connects to every endpoint, and verifies the chain ID and
    /// every token's decimals.
    ///
    /// # Errors
    ///
    /// [`Self::validate`]'s refusals, an unreachable or foreign node, or a
    /// token whose contract disagrees.
    pub async fn connect(&self) -> anyhow::Result<Client> {
        self.open(self.cluster().await?).await
    }

    /// A cluster of the configured endpoints.
    async fn cluster(&self) -> anyhow::Result<Arc<dyn json_rpc::Client>> {
        ensure!(!self.endpoints.is_empty(), "Ethereum needs an endpoint");
        let nodes = self
            .endpoints
            .iter()
            .map(Endpoint::connect)
            .collect::<Result<Vec<_>, _>>()?;
        let cluster = Cluster::new(nodes, rpc::height, Policy::default())
            .await
            .context("Ethereum endpoints")?;
        Ok(Arc::new(Retry::new(cluster)))
    }

    /// Validates, and a verified client over `transport`.
    async fn open(&self, transport: Arc<dyn json_rpc::Client>) -> anyhow::Result<Client> {
        self.validate()?;
        let client = Client::new(transport, self.native());
        client.verify().await?;
        for token in &self.tokens {
            let decimals = client.decimals(&token.contract).await?;
            ensure!(
                decimals == token.decimals,
                "ERC-20 {} reports {decimals} decimals, not the configured {}",
                token.contract,
                token.decimals
            );
        }
        Ok(client)
    }

    /// The chain read through `rpc`.
    #[must_use]
    pub fn chain(&self, rpc: &Client) -> Chain {
        Chain::new(rpc.clone(), self.contracts())
    }

    /// The wallet factory: nonces from `rpc`, fees and gas from `rpc`
    /// cached by its one estimator.
    #[must_use]
    pub fn factory(&self, rpc: &Client) -> Factory {
        let fees = Estimator::new(
            rpc.clone(),
            self.limits,
            self.contracts(),
            Duration::from_millis(self.fee_ttl_ms),
        );
        Factory {
            rpc: rpc.clone(),
            tokens: self.contracts(),
            fees: Arc::new(fees),
        }
    }

    /// Ether and every allowlisted token, in the scale [`Self::connect`]
    /// checks each contract reports.
    #[must_use]
    pub fn assets(&self) -> Assets {
        let network = base::Network::from(&self.native());
        let ether = AssetInfo {
            asset: Asset::native(network.clone()),
            decimals: ETH_DECIMALS,
            name: Some(ETH_NAME.to_owned()),
            ticker: Some(ETH_TICKER.to_owned()),
        };
        let tokens = self.tokens.iter().map(|token| {
            let asset = Asset {
                network: network.clone(),
                id: hex::encode_prefixed(token.contract),
            };
            AssetInfo::token(asset, token.decimals.into())
        });
        std::iter::once(ether).chain(tokens).collect()
    }

    fn contracts(&self) -> Tokens {
        Tokens::new(self.tokens.iter().map(|token| token.contract))
    }
}

/// A contract written as canonical lowercase text.
fn contract<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<alloy_primitives::Address, D::Error> {
    let text = String::deserialize(deserializer)?;
    let contract = text
        .parse::<alloy_primitives::Address>()
        .map_err(|error| de::Error::custom(format!("token contract: {error}")))?;
    if hex::encode_prefixed(contract) != text {
        return Err(de::Error::custom(
            "token contract must be canonical lowercase",
        ));
    }
    Ok(contract)
}

impl base::Config for Config {
    fn network(&self) -> base::Network {
        (&self.native()).into()
    }

    fn client(&self) -> Future<'_, anyhow::Result<Arc<dyn json_rpc::Client>>> {
        Box::pin(self.cluster())
    }

    fn build<'a>(
        &'a self,
        storage: &'a Storage,
        client: Arc<dyn json_rpc::Client>,
    ) -> Future<'a, anyhow::Result<Service>> {
        Box::pin(async move {
            let rpc = self.open(Arc::clone(&client)).await?;
            let indexer = Arc::new(Indexer::new(
                Arc::new(self.chain(&rpc)),
                Arc::clone(&storage.blocks),
                Arc::clone(&storage.indexes),
                Arc::new(Timer(Duration::from_millis(self.interval_ms))),
            ));
            Ok(Service {
                indexer,
                wallets: WalletFactoryBroadcaster {
                    network: (&self.native()).into(),
                    factory: Arc::new(self.factory(&rpc)),
                    broadcaster: Arc::new(rpc),
                },
                assets: self.assets(),
                storage: storage.clone(),
                client,
            })
        })
    }
}
