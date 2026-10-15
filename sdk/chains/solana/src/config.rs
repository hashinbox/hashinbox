//! [`Config`]: one Solana cluster's settings, and the pieces built from them.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, Assets, Future, Indexer, Service, Storage, Timer, WalletFactoryBroadcaster,
};
use json_rpc::{Cluster, Endpoint, Policy, Retry};
use serde::{Deserialize, Deserializer, de};

use crate::{
    Chain, Client, Factory, GenesisHash, Network, SOL_DECIMALS, SOL_NAME, SOL_TICKER, Tokens,
    fee::Estimator, rpc,
};

/// Everything one Solana cluster needs; built in code with [`Config::new`]
/// or read from any `serde` format.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The stable application name of the cluster, such as `mainnet-beta`.
    pub network: String,
    /// The genesis hash every node is checked against.
    pub genesis_hash: GenesisHash,
    /// The nodes: reads go to one, submissions to all.
    pub endpoints: Vec<Endpoint>,
    /// Allowlisted SPL tokens: the only tokens indexed and sent.
    #[serde(default)]
    pub tokens: Vec<Token>,
    /// How long a fetched blockhash and fee price sends, in milliseconds;
    /// default 5,000.
    #[serde(default = "Config::default_fee_ttl_ms")]
    pub fee_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 1,000.
    #[serde(default = "Config::default_interval_ms")]
    pub interval_ms: u64,
}

/// One allowlisted SPL token and the decimals its mint must declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    /// The mint, written as canonical Base58.
    #[serde(deserialize_with = "mint")]
    pub mint: solana_address::Address,
    /// What the mint must declare.
    pub decimals: u8,
}

impl Config {
    /// `network` over `endpoints`, with no tokens.
    #[must_use]
    pub fn new(network: Network, endpoints: Vec<Endpoint>) -> Self {
        Self {
            network: network.slug,
            genesis_hash: network.genesis_hash,
            endpoints,
            tokens: Vec::new(),
            fee_ttl_ms: Self::default_fee_ttl_ms(),
            interval_ms: Self::default_interval_ms(),
        }
    }

    const fn default_fee_ttl_ms() -> u64 {
        5_000
    }

    const fn default_interval_ms() -> u64 {
        1_000
    }

    /// The configured network.
    #[must_use]
    pub fn native(&self) -> Network {
        Network::new(self.genesis_hash.clone(), self.network.as_str())
    }

    /// Rejects the value on its own, without I/O.
    ///
    /// # Errors
    ///
    /// For an invalid network or endpoints, or a zero interval.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        for endpoint in &self.endpoints {
            endpoint.validate()?;
        }
        ensure!(self.interval_ms > 0, "Solana interval must be positive");
        Ok(())
    }

    /// Validates, connects to every endpoint, and verifies the genesis hash,
    /// the Memo-v3 program and every mint's decimals.
    ///
    /// # Errors
    ///
    /// [`Self::validate`]'s refusals, an unreachable or foreign node, or a
    /// mint that disagrees (Token-2022 included).
    pub async fn connect(&self) -> anyhow::Result<Client> {
        self.open(self.cluster().await?).await
    }

    /// A cluster of the configured endpoints.
    async fn cluster(&self) -> anyhow::Result<Arc<dyn json_rpc::Client>> {
        ensure!(!self.endpoints.is_empty(), "Solana needs an endpoint");
        let nodes = self
            .endpoints
            .iter()
            .map(Endpoint::connect)
            .collect::<Result<Vec<_>, _>>()?;
        let cluster = Cluster::new(nodes, rpc::height, Policy::default())
            .await
            .context("Solana endpoints")?;
        Ok(Arc::new(Retry::new(cluster)))
    }

    /// Validates, and a verified client over `transport`.
    async fn open(&self, transport: Arc<dyn json_rpc::Client>) -> anyhow::Result<Client> {
        self.validate()?;
        let client = Client::new(transport, self.native());
        client.verify().await?;
        for token in &self.tokens {
            let decimals = client.decimals(&token.mint).await?;
            ensure!(
                decimals == token.decimals,
                "Solana mint {} declares {decimals} decimals, not the configured {}",
                token.mint,
                token.decimals
            );
        }
        Ok(client)
    }

    /// The chain read through `rpc`.
    #[must_use]
    pub fn chain(&self, rpc: &Client) -> Chain {
        Chain::new(rpc.clone(), self.mints())
    }

    /// The wallet factory: blockhashes and fees from `rpc`, cached by its
    /// one estimator.
    #[must_use]
    pub fn factory(&self, rpc: &Client) -> Factory {
        let fees = Estimator::new(
            rpc.clone(),
            self.mints(),
            Duration::from_millis(self.fee_ttl_ms),
        );
        Factory {
            rpc: rpc.clone(),
            tokens: self.mints(),
            fees: Arc::new(fees),
        }
    }

    /// SOL and every allowlisted token, in the scale [`Self::connect`]
    /// checks each mint declares.
    #[must_use]
    pub fn assets(&self) -> Assets {
        let network = base::Network::from(&self.native());
        let sol = AssetInfo {
            asset: Asset::native(network.clone()),
            decimals: SOL_DECIMALS,
            name: Some(SOL_NAME.to_owned()),
            ticker: Some(SOL_TICKER.to_owned()),
        };
        let tokens = self.tokens.iter().map(|token| {
            let asset = Asset {
                network: network.clone(),
                id: token.mint.to_string(),
            };
            AssetInfo::token(asset, token.decimals.into())
        });
        std::iter::once(sol).chain(tokens).collect()
    }

    fn mints(&self) -> Tokens {
        Tokens::new(self.tokens.iter().copied())
    }
}

/// A mint written as canonical Base58.
fn mint<'de, D: Deserializer<'de>>(deserializer: D) -> Result<solana_address::Address, D::Error> {
    let text = String::deserialize(deserializer)?;
    let mint = text
        .parse::<solana_address::Address>()
        .map_err(|error| de::Error::custom(format!("token mint: {error}")))?;
    if mint.to_string() != text {
        return Err(de::Error::custom("token mint must be canonical Base58"));
    }
    Ok(mint)
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
