//! [`Config`]: one Ethereum network's settings; [`Factory`]: the pieces
//! built from them, a stored network's service among them.

use std::{sync::Arc, time::Duration};

use alloy_primitives::hex;
use anyhow::{Context as _, ensure};
use base::{
    Asset, AssetInfo, AssetKind, Assets, FeeRateProviderCached, Indexer, NetworkConfig, Service,
    Storage, Tracking, TransactionIndexer, WalletFactoryBroadcaster, WalletIndexer,
};
use serde::Deserialize;

use crate::{
    CHAIN_NAME, Chain, Client, ETH_DECIMALS, ETH_NAME, ETH_TICKER, FeeLimits, Network, Tokens,
    WalletFactory, fee_estimator::Estimator, fee_rate::RateProvider,
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
    /// How often the indexer wakes, in milliseconds; default 12,000.
    pub interval_ms: u64,
    /// Blocks the indexer fetches per step; default 100
    /// ([`base::DEFAULT_BATCH`]). Lower it for a node that limits requests.
    pub batch: u64,
    /// Blocks a block needs below the tip to count as confirmed: the
    /// confirmed position is `tip - confirmations`; default 64 (about two
    /// epochs, where `finalized` sits).
    pub confirmations: u64,
    /// How sent payments are checked and bumped; default every 30 s, after
    /// 50 blocks, at most 5 times.
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
            fee_ttl_ms: 12_000,
            interval_ms: 12_000,
            batch: base::DEFAULT_BATCH,
            check_ms: 30_000,
            bump_after: 50,
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
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            confirmations: 64,
            tracking: settings.tracking(),
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
    /// For an invalid network, a zero token contract, or contradicting fee
    /// limits.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.native().validate()?;
        ensure!(
            self.tokens.iter().all(|token| !token.contract.is_zero()),
            "Ethereum token contract must not be the zero address"
        );
        ensure!(self.interval_ms > 0, "Ethereum interval must be positive");
        self.limits.validate()
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
        Tokens::new(
            (&self.native()).into(),
            self.tokens
                .iter()
                .map(|token| (token.contract, token.decimals.into())),
        )
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

impl TryFrom<&NetworkConfig> for Config {
    type Error = anyhow::Error;

    /// Its chain ID, assets and limits, and its knobs from
    /// `settings`.
    fn try_from(config: &NetworkConfig) -> anyhow::Result<Self> {
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
            interval_ms: settings.interval_ms,
            batch: settings.batch,
            confirmations: config.confirmations,
            tracking: settings.tracking(),
        };
        ensure!(
            base::Network::from(&built.native()) == config.network,
            "not an Ethereum network"
        );
        Ok(built)
    }
}

/// Builds Ethereum's pieces from a [`Config`]; as its
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
        Chain::new(
            rpc.clone(),
            self.config.contracts(),
            self.config.confirmations,
        )
    }

    /// The wallet factory over `rpc`: nonces from `rpc`, after the sends in
    /// flight `transactions` holds; its fee rate and gas from `rpc`, kept
    /// `fee_ttl_ms`, each send priced for its recipient as `holdings` say.
    #[must_use]
    pub fn wallets(
        &self,
        rpc: &Client,
        holdings: Arc<dyn base::Holdings>,
        transactions: Arc<dyn base::TransactionIndexStore>,
    ) -> WalletFactory {
        let config = &self.config;
        let provider = RateProvider {
            network: rpc.network().into(),
            rpc: rpc.clone(),
        };
        let rates = Arc::new(FeeRateProviderCached::new(
            Arc::new(provider),
            Duration::from_millis(config.fee_ttl_ms),
        ));
        let fees = Estimator::new(
            rpc.clone(),
            config.limits,
            config.contracts(),
            rates,
            holdings,
            Duration::from_millis(config.fee_ttl_ms),
        );
        WalletFactory {
            rpc: rpc.clone(),
            transactions,
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
        let factory = self.wallets(
            &rpc,
            Arc::clone(&storage.holdings),
            Arc::clone(&storage.transaction_indexes),
        );
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
