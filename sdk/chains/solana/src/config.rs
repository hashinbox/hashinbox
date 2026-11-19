//! [`Config`]: one Solana cluster's settings; [`Factory`]: the pieces
//! built from them, a stored network's service among them.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, AssetKind, Assets, FeeRateProviderCached, Indexer, NetworkConfig, Service,
    Storage, Tracking, TransactionIndexer, WalletFactoryBroadcaster, WalletIndexer,
};
use serde::Deserialize;

use crate::{
    CHAIN_NAME, Chain, Client, GenesisHash, Network, SOL_DECIMALS, SOL_NAME, SOL_TICKER, Tokens,
    WalletFactory, fee_estimator::Estimator, fee_rate::RateProvider,
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
    /// How long a fetched signature price and priority-fee rate price
    /// sends, in milliseconds; default 5,000.
    pub fee_ttl_ms: u64,
    /// The most a transaction pays per compute unit to be scheduled first,
    /// in micro-lamports; above it, signing fails as `FeeCap`. Default
    /// 1,000,000 (one lamport): a priority fee of at most 0.00004 SOL for a
    /// SOL transfer and 0.0001 SOL for a token one, far above what a
    /// payment's accounts (a sender and its recipient, rarely contended)
    /// usually see, so it stops a runaway quote or bump, not a busy network.
    pub max_priority_fee: u64,
    /// How often the indexer wakes, in milliseconds; default 1,000.
    pub interval_ms: u64,
    /// Blocks the indexer fetches per step; default 100
    /// ([`base::DEFAULT_BATCH`]). Lower it for a node that limits requests.
    pub batch: u64,
    /// Slots below the tip the indexer reads; an index further behind
    /// settles by a balance read. Default 1,000 ([`crate::READ_DEPTH`]).
    pub read_depth: u64,
    /// Slots a block needs below the tip to count as confirmed: the
    /// confirmed position is `tip - confirmations`; default 32.
    pub confirmations: u64,
    /// How sent payments are checked and bumped; default every 10 s, at most
    /// 5 bumps, each once the payment expired unseen (`bump_after` unused).
    pub tracking: Tracking,
}

/// The knobs a [`base::NetworkConfig`]'s `settings` may set: `fee_ttl_ms`,
/// `max_priority_fee`, `interval_ms`, `batch`, `read_depth`, and the tracking's
/// `check_ms`, `bump_after`, `max_bumps`; an absent one keeps its default.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    fee_ttl_ms: u64,
    max_priority_fee: u64,
    interval_ms: u64,
    batch: u64,
    read_depth: u64,
    check_ms: u64,
    bump_after: u64,
    max_bumps: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fee_ttl_ms: 5_000,
            max_priority_fee: 1_000_000,
            interval_ms: 1_000,
            batch: base::DEFAULT_BATCH,
            read_depth: crate::READ_DEPTH,
            check_ms: 10_000,
            bump_after: 0,
            max_bumps: 5,
        }
    }
}

impl Settings {
    fn tracking(&self) -> Tracking {
        Tracking {
            check: Duration::from_millis(self.check_ms),
            bump_after: self.bump_after,
            max_bumps: self.max_bumps,
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
            max_priority_fee: settings.max_priority_fee,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            read_depth: settings.read_depth,
            confirmations: 32,
            tracking: settings.tracking(),
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
    /// For an invalid network, a zero interval or a zero read depth.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        ensure!(self.interval_ms > 0, "Solana interval must be positive");
        ensure!(self.read_depth > 0, "Solana read_depth must be positive");
        Ok(())
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
        Tokens::new(
            (&self.native()).into(),
            self.tokens
                .iter()
                .map(|token| (token.mint, token.decimals.into())),
        )
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

impl TryFrom<&NetworkConfig> for Config {
    type Error = anyhow::Error;

    /// Its genesis hash (`chain_id`) and assets, and its knobs
    /// from `settings`.
    fn try_from(config: &NetworkConfig) -> anyhow::Result<Self> {
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
            max_priority_fee: settings.max_priority_fee,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            read_depth: settings.read_depth,
            confirmations: config.confirmations,
            tracking: settings.tracking(),
        };
        ensure!(
            base::Network::from(&built.native()) == config.network,
            "not a Solana network"
        );
        Ok(built)
    }
}

/// Builds Solana's pieces from a [`Config`]; as its
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
    #[must_use]
    pub fn chain(&self, rpc: &Client) -> Chain {
        Chain::new(rpc.clone(), self.config.mints()).with_read_depth(self.config.read_depth)
    }

    /// The wallet factory over `rpc`: blockhashes, fees and the priority-fee
    /// rate from `rpc`, the fees and rate kept `fee_ttl_ms`, priced under
    /// `max_priority_fee`, token recipients' accounts as `holdings` say.
    #[must_use]
    pub fn wallets(&self, rpc: &Client, holdings: Arc<dyn base::Holdings>) -> WalletFactory {
        let config = &self.config;
        let ttl = Duration::from_millis(config.fee_ttl_ms);
        let provider = RateProvider {
            network: rpc.network().into(),
            rpc: rpc.clone(),
        };
        let rates = Arc::new(FeeRateProviderCached::new(Arc::new(provider), ttl));
        let fees = Estimator::new(
            rpc.clone(),
            config.mints(),
            config.max_priority_fee,
            rates,
            holdings,
            ttl,
        );
        WalletFactory {
            rpc: rpc.clone(),
            tokens: config.mints(),
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
        let rpc = Client::new(Arc::clone(&client), config.native());
        let chain = Arc::new(self.chain(&rpc));
        let wallets = WalletIndexer::new(
            Arc::clone(&storage.indexes),
            chain.clone(),
            Some(chain.clone()),
        );
        let transactions = TransactionIndexer::new(
            Arc::clone(&storage.transaction_indexes),
            chain.clone(),
            Some(chain.clone()),
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
        let factory = self.wallets(&rpc, Arc::clone(&storage.holdings));
        let rates = Arc::clone(&factory.fees.rates);
        Ok(Service {
            indexer,
            wallets: WalletFactoryBroadcaster {
                network: (&config.native()).into(),
                factory: Arc::new(factory),
                broadcaster: Arc::new(rpc),
            },
            assets: config.assets(),
            storage: storage.clone(),
            client,
            dated: Some(chain.clone()),
            balances: Some(chain.clone()),
            confirmer: Some(chain),
            confirmations: config.confirmations,
            tracking: config.tracking,
            rates,
        })
    }
}
