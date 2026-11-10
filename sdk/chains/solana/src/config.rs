//! [`Config`]: one Solana cluster's settings, and the pieces built from them.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, AssetKind, Assets, Future, Indexer, Service, Storage,
    WalletFactoryBroadcaster,
};
use serde::Deserialize;

use crate::{
    Chain, Client, Factory, GenesisHash, Network, SOL_DECIMALS, SOL_NAME, SOL_TICKER, Tokens,
    fee::Estimator, rpc,
};

/// Everything one Solana cluster needs; built in code with [`Config::new`]
/// or from a [`base::NetworkConfig`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The stable application name of the cluster, such as `mainnet-beta`.
    pub network: String,
    /// The genesis hash every node is checked against.
    pub genesis_hash: GenesisHash,
    /// Allowlisted SPL tokens: the only tokens indexed and sent.
    pub tokens: Vec<Token>,
    /// Fractional digits a view shows of the native currency; none: all.
    pub display_decimals: Option<u32>,
    /// How long a fetched blockhash and fee price sends, in milliseconds;
    /// default 5,000.
    pub fee_ttl_ms: u64,
    /// How long a broadcast transaction counts as pending before it is
    /// given up on, in milliseconds; default 120_000 (two minutes, past a blockhash's life).
    pub pending_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 1,000.
    pub interval_ms: u64,
    /// Blocks the indexer fetches per step; default 100
    /// ([`base::DEFAULT_BATCH`]). Lower it for a node that limits requests.
    pub batch: u64,
    /// How old a watch's start may be: about what history the nodes keep;
    /// none: no limit.
    pub max_history: Option<Duration>,
    /// Slots a block needs below the tip to count as confirmed: the
    /// confirmed position is `tip - confirmations`; default 32.
    pub confirmations: u64,
}

/// The knobs a [`base::NetworkConfig`]'s `settings` may set: `fee_ttl_ms`,
/// `pending_ttl_ms`, `interval_ms`, `batch`; an absent one keeps its default.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    fee_ttl_ms: u64,
    pending_ttl_ms: u64,
    interval_ms: u64,
    batch: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fee_ttl_ms: 5_000,
            pending_ttl_ms: 120_000,
            interval_ms: 1_000,
            batch: base::DEFAULT_BATCH,
        }
    }
}

/// One allowlisted SPL token and the decimals its mint must declare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The mint.
    pub mint: solana_address::Address,
    /// What the mint must declare.
    pub decimals: u8,
    /// Its display symbol, such as `USDC`; none: shown by its mint.
    pub ticker: Option<String>,
    /// Fractional digits a view shows; none: all.
    pub display_decimals: Option<u32>,
}

impl Config {
    /// `network`, with no tokens.
    #[must_use]
    pub fn new(network: Network) -> Self {
        let settings = Settings::default();
        Self {
            network: network.slug,
            genesis_hash: network.genesis_hash,
            tokens: Vec::new(),
            display_decimals: None,
            fee_ttl_ms: settings.fee_ttl_ms,
            pending_ttl_ms: settings.pending_ttl_ms,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            max_history: None,
            confirmations: 32,
        }
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
    /// For an invalid network, or a zero interval.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        ensure!(self.interval_ms > 0, "Solana interval must be positive");
        Ok(())
    }

    /// Validates, and a client over `transport`; asks the node nothing.
    fn open(&self, transport: Arc<dyn ::rpc::Client>) -> anyhow::Result<Client> {
        self.validate()?;
        Ok(Client::new(transport, self.native()))
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
            pending_ttl: Duration::from_millis(self.pending_ttl_ms),
            tokens: self.mints(),
            fees: Arc::new(fees),
        }
    }

    /// SOL and every allowlisted token, in its configured decimals.
    #[must_use]
    pub fn assets(&self) -> Assets {
        let network = base::Network::from(&self.native());
        let sol = AssetInfo {
            asset: Asset::native(network.clone()),
            decimals: SOL_DECIMALS,
            name: Some(SOL_NAME.to_owned()),
            ticker: Some(SOL_TICKER.to_owned()),
            display_decimals: self.display_decimals,
        };
        let tokens = self.tokens.iter().map(|token| {
            let asset = Asset {
                network: network.clone(),
                id: token.mint.to_string(),
            };
            AssetInfo {
                ticker: token.ticker.clone(),
                display_decimals: token.display_decimals,
                ..AssetInfo::token(asset, token.decimals.into())
            }
        });
        std::iter::once(sol).chain(tokens).collect()
    }

    fn mints(&self) -> Tokens {
        Tokens::new(self.tokens.iter().cloned())
    }
}

/// A mint written as canonical Base58.
fn mint(text: &str) -> anyhow::Result<solana_address::Address> {
    let mint = text
        .parse::<solana_address::Address>()
        .map_err(|error| anyhow::anyhow!("token mint {text}: {error}"))?;
    ensure!(
        mint.to_string() == text,
        "token mint must be canonical Base58"
    );
    Ok(mint)
}

impl TryFrom<&base::NetworkConfig> for Config {
    type Error = anyhow::Error;

    /// Its genesis hash (`chain_id`) and assets, and its knobs
    /// from `settings`.
    fn try_from(config: &base::NetworkConfig) -> anyhow::Result<Self> {
        let settings = Settings::deserialize(&config.settings).context("Solana settings")?;
        let genesis = config
            .chain_id
            .as_deref()
            .context("Solana needs a chain_id")?;
        let mut tokens = Vec::new();
        let mut display_decimals = None;
        for asset in &config.assets {
            if asset.kind == AssetKind::Native {
                ensure!(asset.decimals == SOL_DECIMALS, "SOL has 9 decimals");
                display_decimals = asset.display_decimals;
                continue;
            }
            let address = asset
                .contract
                .as_deref()
                .context("a token needs its mint")?;
            tokens.push(Token {
                mint: mint(address)?,
                decimals: u8::try_from(asset.decimals).context("token decimals")?,
                ticker: Some(asset.ticker.clone()),
                display_decimals: asset.display_decimals,
            });
        }
        let built = Self {
            network: config.network.name.clone(),
            genesis_hash: genesis.parse().context("Solana genesis hash")?,
            tokens,
            display_decimals,
            fee_ttl_ms: settings.fee_ttl_ms,
            pending_ttl_ms: settings.pending_ttl_ms,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            max_history: config.max_history,
            confirmations: config.confirmations,
        };
        ensure!(
            base::Config::network(&built) == config.network,
            "not a Solana network"
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
                    factory: Arc::new(self.factory(&rpc)),
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
