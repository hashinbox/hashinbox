//! [`Config`]: one Tron network's settings; [`Factory`]: the pieces built
//! from them, a stored network's service among them.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, AssetKind, Assets, Indexer, NetworkConfig, Service, Storage, Tracking,
    TransactionIndexer, WalletFactoryBroadcaster, WalletIndexer,
};
use serde::Deserialize;

use crate::{
    Account, CHAIN_NAME, Chain, Client, FeeLimits, Network, TRX_DECIMALS, TRX_NAME, TRX_TICKER,
    Tokens, WalletFactory, fee::Estimator,
};

/// Everything one Tron network needs; built in code with [`Config::new`] or
/// from a [`base::NetworkConfig`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The stable application name of the network, such as `mainnet`.
    pub network: String,
    /// The genesis block id every node is checked against.
    pub genesis: String,
    /// Allowlisted TRC-20 tokens: the only tokens indexed and sent.
    pub tokens: Vec<Token>,
    /// Fractional digits a view shows of the native currency; none: all.
    pub display_decimals: Option<u32>,
    /// What a token transfer may burn.
    pub limits: FeeLimits,
    /// How long fetched resource prices price sends, in milliseconds;
    /// default 60,000.
    pub fee_ttl_ms: u64,
    /// How often the indexer wakes, in milliseconds; default 3,000.
    pub interval_ms: u64,
    /// Blocks the indexer fetches per step; default 100
    /// ([`base::DEFAULT_BATCH`]), two requests each. Lower it for a node
    /// that limits requests.
    pub batch: u64,
    /// Blocks a block needs below the tip to count as confirmed; default
    /// 19, about where the solidified block sits.
    pub confirmations: u64,
    /// Blocks below the tip a watch may start, at least `confirmations`;
    /// none: no limit.
    pub max_depth: Option<u64>,
    /// How sent payments are checked and signed again; default every 10 s,
    /// at most 5 new versions, each once the last expired unseen
    /// (`bump_after` unused).
    pub tracking: Tracking,
}

/// The knobs a [`base::NetworkConfig`]'s `settings` may set: `limits`,
/// `fee_ttl_ms`, `interval_ms`, `batch`, and the tracking's `check_ms`,
/// `bump_after`, `max_bumps`; an absent one keeps its default.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    limits: FeeLimits,
    fee_ttl_ms: u64,
    interval_ms: u64,
    batch: u64,
    check_ms: u64,
    bump_after: u64,
    max_bumps: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            limits: FeeLimits::default(),
            fee_ttl_ms: 60_000,
            interval_ms: 3_000,
            batch: base::DEFAULT_BATCH,
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

/// One allowlisted TRC-20 token and the decimals its contract must report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The contract.
    pub contract: Account,
    /// What its `decimals()` must report.
    pub decimals: u8,
    /// Its display symbol, such as `USDT`; none: shown by its contract.
    pub ticker: Option<String>,
    /// Fractional digits a view shows; none: all.
    pub display_decimals: Option<u32>,
}

impl Config {
    /// `network`, with no tokens and default limits.
    #[must_use]
    pub fn new(network: Network) -> Self {
        let settings = Settings::default();
        Self {
            network: network.slug,
            genesis: network.genesis,
            tokens: Vec::new(),
            display_decimals: None,
            limits: settings.limits,
            fee_ttl_ms: settings.fee_ttl_ms,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            confirmations: 19,
            max_depth: None,
            tracking: settings.tracking(),
        }
    }

    /// The configured network.
    #[must_use]
    pub fn native(&self) -> Network {
        Network::new(self.genesis.as_str(), self.network.as_str())
    }

    /// Rejects the value on its own, without I/O.
    ///
    /// # Errors
    ///
    /// For an invalid network, a zero interval or limit, or a `max_depth`
    /// below `confirmations`.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        ensure!(self.interval_ms > 0, "Tron interval must be positive");
        ensure!(
            self.limits.energy > 0 && self.limits.max_fee_limit > 0,
            "Tron fee limits must be greater than zero"
        );
        ensure!(
            self.max_depth
                .is_none_or(|depth| depth >= self.confirmations),
            "Tron max_depth must be at least its confirmations ({})",
            self.confirmations
        );
        Ok(())
    }

    /// TRX and every allowlisted token, in its configured decimals.
    #[must_use]
    pub fn assets(&self) -> Assets {
        let network = base::Network::from(&self.native());
        let trx = AssetInfo {
            asset: Asset::native(network.clone()),
            decimals: TRX_DECIMALS,
            name: Some(TRX_NAME.to_owned()),
            ticker: Some(TRX_TICKER.to_owned()),
            display_decimals: self.display_decimals,
        };
        let tokens = self.tokens.iter().map(|token| {
            let asset = Asset {
                network: network.clone(),
                id: token.contract.to_string(),
            };
            AssetInfo {
                ticker: token.ticker.clone(),
                display_decimals: token.display_decimals,
                ..AssetInfo::token(asset, token.decimals.into())
            }
        });
        std::iter::once(trx).chain(tokens).collect()
    }

    fn contracts(&self) -> Tokens {
        Tokens::new(
            (&self.native()).into(),
            self.tokens
                .iter()
                .map(|token| (token.contract, token.decimals.into())),
        )
    }
}

impl TryFrom<&NetworkConfig> for Config {
    type Error = anyhow::Error;

    /// Its genesis id (`chain_id`), assets, confirmations and `max_depth`,
    /// and its knobs from `settings`.
    fn try_from(config: &NetworkConfig) -> anyhow::Result<Self> {
        let settings = Settings::deserialize(&config.settings).context("Tron settings")?;
        let genesis = config
            .chain_id
            .as_deref()
            .context("Tron needs a chain_id: its genesis block id")?;
        let mut tokens = Vec::new();
        let mut display_decimals = None;
        for asset in &config.assets {
            if asset.kind == AssetKind::Native {
                ensure!(asset.decimals == TRX_DECIMALS, "TRX has 6 decimals");
                display_decimals = asset.display_decimals;
                continue;
            }
            let text = asset
                .contract
                .as_deref()
                .context("a token needs its contract")?;
            let contract: Account = text
                .parse()
                .with_context(|| format!("token contract {text}"))?;
            ensure!(
                contract.to_string() == text,
                "token contract must be its T… text"
            );
            tokens.push(Token {
                contract,
                decimals: u8::try_from(asset.decimals).context("token decimals")?,
                ticker: Some(asset.ticker.clone()),
                display_decimals: asset.display_decimals,
            });
        }
        let built = Self {
            network: config.network.name.clone(),
            genesis: genesis.to_owned(),
            tokens,
            display_decimals,
            limits: settings.limits,
            fee_ttl_ms: settings.fee_ttl_ms,
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            confirmations: config.confirmations,
            max_depth: config.max_depth,
            tracking: settings.tracking(),
        };
        ensure!(
            base::Network::from(&built.native()) == config.network,
            "not a Tron network"
        );
        Ok(built)
    }
}

/// Builds Tron's pieces from a [`Config`]; as its [`base::ServiceFactory`],
/// a stored network's whole service.
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
        let config = &self.config;
        Chain::new(
            rpc.clone(),
            config.contracts(),
            config.confirmations,
            config.max_depth,
        )
    }

    /// The wallet factory over `rpc`: resource prices from `rpc`, kept
    /// `fee_ttl_ms`, under the fee limits, each TRX send priced for its
    /// recipient's account as `holdings` say.
    #[must_use]
    pub fn wallets(&self, rpc: &Client, holdings: Arc<dyn base::Holdings>) -> WalletFactory {
        let config = &self.config;
        let fees = Estimator::new(
            rpc.clone(),
            config.limits,
            config.contracts(),
            holdings,
            Duration::from_millis(config.fee_ttl_ms),
        );
        WalletFactory {
            rpc: rpc.clone(),
            tokens: config.contracts(),
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
    /// the nodes through `client`; asks no node. It reads no balance and
    /// confirms no transaction by id: an index is read block by block.
    fn service(
        &self,
        storage: &Storage,
        client: Arc<dyn ::rpc::Client>,
    ) -> anyhow::Result<Service> {
        let config = &self.config;
        config.validate()?;
        let rpc = Client::new(Arc::clone(&client), config.native());
        let chain = Arc::new(self.chain(&rpc));
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
        let factory = self.wallets(&rpc, Arc::clone(&storage.holdings));
        let rates = factory.fees.clone();
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
            dated: Some(chain),
            balances: None,
            confirmer: None,
            confirmations: config.confirmations,
            tracking: config.tracking,
            rates,
        })
    }
}
