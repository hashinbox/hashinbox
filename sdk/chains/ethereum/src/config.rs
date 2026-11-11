//! [`Config`]: one Ethereum network's settings, and the pieces built from them.

use std::{sync::Arc, time::Duration};

use alloy_primitives::hex;
use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, AssetKind, Assets, Future, Indexer, Service, Storage,
    WalletFactoryBroadcaster,
};
use serde::Deserialize;

use crate::{
    Chain, Client, ETH_DECIMALS, ETH_NAME, ETH_TICKER, Factory, FeeLimits, Network, Tokens,
    fee::Estimator, fee_cache::Cached, rpc,
};

/// Everything one Ethereum network needs; built in code with
/// [`Config::new`] or from a [`base::NetworkConfig`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The stable application name of the network, such as `mainnet`.
    pub network: String,
    /// The EIP-155 chain ID the node is checked against.
    pub chain_id: u64,
    /// Allowlisted ERC-20 tokens: the only tokens indexed and sent.
    pub tokens: Vec<Token>,
    /// Fractional digits a view shows of the native currency; none: all.
    pub display_decimals: Option<u32>,
    /// Ceilings on the fees a transaction is built under.
    pub limits: FeeLimits,
    /// How long fetched fees price sends, in milliseconds; default 12,000.
    pub fee_ttl_ms: u64,
    /// How long a broadcast transaction counts as pending before it is
    /// given up on, in milliseconds; default 3_600_000 (one hour).
    pub pending_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 12,000.
    pub interval_ms: u64,
    /// Blocks the indexer fetches per step; default 100
    /// ([`base::DEFAULT_BATCH`]). Lower it for a node that limits requests.
    pub batch: u64,
    /// How old a watch's start may be: about what history the nodes keep;
    /// none: no limit.
    pub max_history: Option<Duration>,
    /// Blocks a block needs below the tip to count as confirmed: the
    /// confirmed position is `tip - confirmations`; default 12.
    pub confirmations: u64,
}

/// The knobs a [`base::NetworkConfig`]'s `settings` may set: `limits`,
/// `fee_ttl_ms`, `pending_ttl_ms`, `interval_ms`, `batch`; an absent one
/// keeps its default.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    limits: FeeLimits,
    fee_ttl_ms: u64,
    pending_ttl_ms: u64,
    interval_ms: u64,
    batch: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            limits: FeeLimits::default(),
            fee_ttl_ms: 12_000,
            pending_ttl_ms: 3_600_000,
            interval_ms: 12_000,
            batch: base::DEFAULT_BATCH,
        }
    }
}

/// One allowlisted ERC-20 token and the decimals its contract must report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The contract.
    pub contract: alloy_primitives::Address,
    /// What its `decimals()` must report.
    pub decimals: u8,
    /// Its display symbol, such as `USDC`; none: shown by its contract.
    pub ticker: Option<String>,
    /// Fractional digits a view shows; none: all.
    pub display_decimals: Option<u32>,
}

impl Config {
    /// `network`, with no tokens and default fee limits.
    #[must_use]
    pub fn new(network: Network) -> Self {
        let settings = Settings::default();
        Self {
            network: network.slug,
            chain_id: network.chain_id,
            tokens: Vec::new(),
            display_decimals: None,
            limits: settings.limits,
            fee_ttl_ms: settings.fee_ttl_ms,
            pending_ttl_ms: settings.pending_ttl_ms,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            max_history: None,
            confirmations: 12,
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
    /// For an invalid network, a zero token
    /// contract, or contradicting fee limits.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        ensure!(
            self.tokens.iter().all(|token| !token.contract.is_zero()),
            "Ethereum token contract must not be the zero address"
        );
        ensure!(self.interval_ms > 0, "Ethereum interval must be positive");
        self.limits.validate()
    }

    /// Validates, and a client over `transport`; asks the node nothing.
    fn open(&self, transport: Arc<dyn ::rpc::Client>) -> anyhow::Result<Client> {
        self.validate()?;
        Ok(Client::new(transport, self.native()))
    }

    /// The chain read through `rpc`.
    #[must_use]
    pub fn chain(&self, rpc: &Client) -> Chain {
        Chain::new(rpc.clone(), self.contracts())
    }

    /// The wallet factory: nonces from `rpc`; fees and gas from `rpc`,
    /// reused by its one cached estimator for `fee_ttl_ms`, each send priced
    /// for its recipient as `holdings` say.
    #[must_use]
    pub fn factory(&self, rpc: &Client, holdings: Arc<dyn base::Holdings>) -> Factory {
        let fees = Cached::new(
            Estimator::new(rpc.clone(), self.limits, self.contracts()),
            holdings,
            Duration::from_millis(self.fee_ttl_ms),
        );
        Factory {
            rpc: rpc.clone(),
            pending_ttl: Duration::from_millis(self.pending_ttl_ms),
            tokens: self.contracts(),
            fees: Arc::new(fees),
        }
    }

    /// Ether and every allowlisted token, in its configured decimals.
    #[must_use]
    pub fn assets(&self) -> Assets {
        let network = base::Network::from(&self.native());
        let ether = AssetInfo {
            asset: Asset::native(network.clone()),
            decimals: ETH_DECIMALS,
            name: Some(ETH_NAME.to_owned()),
            ticker: Some(ETH_TICKER.to_owned()),
            display_decimals: self.display_decimals,
        };
        let tokens = self.tokens.iter().map(|token| {
            let asset = Asset {
                network: network.clone(),
                id: hex::encode_prefixed(token.contract),
            };
            AssetInfo {
                ticker: token.ticker.clone(),
                display_decimals: token.display_decimals,
                ..AssetInfo::token(asset, token.decimals.into())
            }
        });
        std::iter::once(ether).chain(tokens).collect()
    }

    fn contracts(&self) -> Tokens {
        Tokens::new(self.tokens.iter().map(|token| token.contract))
    }
}

/// A contract written as canonical lowercase text.
fn contract(text: &str) -> anyhow::Result<alloy_primitives::Address> {
    let contract = text
        .parse::<alloy_primitives::Address>()
        .with_context(|| format!("token contract {text}"))?;
    ensure!(
        hex::encode_prefixed(contract) == text,
        "token contract must be canonical lowercase"
    );
    Ok(contract)
}

impl TryFrom<&base::NetworkConfig> for Config {
    type Error = anyhow::Error;

    /// Its chain ID, assets and limits, and its knobs from
    /// `settings`.
    fn try_from(config: &base::NetworkConfig) -> anyhow::Result<Self> {
        let settings = Settings::deserialize(&config.settings).context("Ethereum settings")?;
        let chain_id = config
            .chain_id
            .as_deref()
            .context("Ethereum needs a chain_id")?;
        let mut tokens = Vec::new();
        let mut display_decimals = None;
        for asset in &config.assets {
            if asset.kind == AssetKind::Native {
                ensure!(asset.decimals == ETH_DECIMALS, "ether has 18 decimals");
                display_decimals = asset.display_decimals;
                continue;
            }
            let address = asset
                .contract
                .as_deref()
                .context("a token needs its contract")?;
            tokens.push(Token {
                contract: contract(address)?,
                decimals: u8::try_from(asset.decimals).context("token decimals")?,
                ticker: Some(asset.ticker.clone()),
                display_decimals: asset.display_decimals,
            });
        }
        let built = Self {
            network: config.network.name.clone(),
            chain_id: chain_id.parse().context("Ethereum chain_id")?,
            tokens,
            display_decimals,
            limits: settings.limits,
            fee_ttl_ms: settings.fee_ttl_ms,
            pending_ttl_ms: settings.pending_ttl_ms,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            max_history: config.max_history,
            confirmations: config.confirmations,
        };
        ensure!(
            base::Config::network(&built) == config.network,
            "not an Ethereum network"
        );
        Ok(built)
    }
}

impl base::Config for Config {
    fn network(&self) -> base::Network {
        (&self.native()).into()
    }

    fn height(&self) -> base::Height {
        rpc::height
    }

    fn info(&self, client: Arc<dyn ::rpc::Client>) -> Future<'_, anyhow::Result<base::NodeInfo>> {
        Box::pin(async move { self.open(client)?.info().await })
    }

    fn build<'a>(
        &'a self,
        storage: &'a Storage,
        client: Arc<dyn ::rpc::Client>,
    ) -> Future<'a, anyhow::Result<Service>> {
        Box::pin(async move {
            let rpc = self.open(Arc::clone(&client))?;
            let chain = Arc::new(self.chain(&rpc));
            let indexer = Arc::new(
                Indexer::new(
                    chain.clone(),
                    Arc::clone(&storage.blocks),
                    Arc::clone(&storage.indexes),
                    Duration::from_millis(self.interval_ms),
                )
                .with_batch(self.batch),
            );
            Ok(Service {
                indexer,
                wallets: WalletFactoryBroadcaster {
                    network: (&self.native()).into(),
                    factory: Arc::new(self.factory(&rpc, Arc::clone(&storage.holdings))),
                    broadcaster: Arc::new(rpc),
                },
                assets: self.assets(),
                storage: storage.clone(),
                client,
                dated: Some(chain),
                max_history: self.max_history,
                confirmations: self.confirmations,
            })
        })
    }
}
