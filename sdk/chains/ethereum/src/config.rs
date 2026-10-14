//! [`Config`]: one Ethereum network's settings, and the pieces built from them.

use std::sync::Arc;

use alloy_primitives::hex;
use anyhow::{Context as _, ensure};
use base::{Asset, AssetInfo, Assets};
use json_rpc::{Cluster, Endpoint, Policy, Retry};
use serde::{Deserialize, Deserializer, de};

use crate::{
    Chain, Client, ETH_DECIMALS, ETH_NAME, ETH_TICKER, Factory, FeeLimits, Network, Tokens, rpc,
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
        }
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
    /// For an invalid network, no or invalid endpoints, a zero token
    /// contract, or contradicting fee limits.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        ensure!(!self.endpoints.is_empty(), "Ethereum needs an endpoint");
        for endpoint in &self.endpoints {
            endpoint.validate()?;
        }
        ensure!(
            self.tokens.iter().all(|token| !token.contract.is_zero()),
            "Ethereum token contract must not be the zero address"
        );
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
        self.validate()?;
        let nodes = self
            .endpoints
            .iter()
            .map(Endpoint::connect)
            .collect::<Result<Vec<_>, _>>()?;
        let cluster = Cluster::new(nodes, rpc::height, Policy::default())
            .await
            .context("Ethereum endpoints")?;
        let client = Client::new(Arc::new(Retry::new(cluster)), self.native());
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

    /// The wallet factory: nonces and fees from `rpc`.
    #[must_use]
    pub fn factory(&self, rpc: &Client) -> Factory {
        Factory {
            rpc: rpc.clone(),
            limits: self.limits,
            tokens: self.contracts(),
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
