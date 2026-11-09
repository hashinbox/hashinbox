//! [`Payments`]: every configured network built over the application's
//! stores, and its [`Builder`].

use std::{collections::BTreeMap, fmt, sync::Arc};

use anyhow::{Context as _, ensure};
use base::{
    Address, Asset, Assets, Birthday, Chain, Config, Network, NetworkConfig, Service, Stats,
    Storage, TransactionError, TransactionErrorKind, Wallet, WalletError, WalletErrorKind,
    WalletFactories,
};
use crypto::SecretBytes;
use rpc::Swap;

/// Builds a registered chain's configuration from a [`NetworkConfig`].
type Parser = Box<dyn Fn(&NetworkConfig) -> anyhow::Result<Box<dyn Config>> + Send + Sync>;

/// Every configured network's [`Service`], and the swappable transport it
/// reaches its nodes through; every call is keyed by network.
///
/// It names no chain: the application registers each chain's configuration
/// with [`Builder::with_chain`] and hands [`NetworkConfig`]s in with
/// [`Builder::with_network_config`], or chain configurations with
/// [`Builder::with_network`]. A network that did not build has no service;
/// asking for it says why.
pub struct Payments {
    services: BTreeMap<Network, Service>,
    clients: BTreeMap<Network, Arc<Swap>>,
    /// Why each network that did not build did not.
    failures: BTreeMap<Network, String>,
}

impl Payments {
    /// A builder with nothing configured.
    #[must_use]
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// The service of `network`.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a network that is not configured, or that did not
    /// build (with why).
    pub fn service(&self, network: &Network) -> anyhow::Result<&Service> {
        if let Some(service) = self.services.get(network) {
            return Ok(service);
        }
        let Some(reason) = self.failures.get(network) else {
            return Err(unsupported().into());
        };
        Err(WalletError::new(
            WalletErrorKind::Unsupported,
            format!("network did not build: {reason}"),
        )
        .into())
    }

    /// Every network's service.
    #[must_use]
    pub const fn services(&self) -> &BTreeMap<Network, Service> {
        &self.services
    }

    /// The configured networks, in order.
    pub fn networks(&self) -> impl Iterator<Item = &Network> {
        self.services.keys()
    }

    /// Every network's native currency and configured tokens.
    #[must_use]
    pub fn assets(&self) -> Assets {
        self.services
            .values()
            .flat_map(|service| service.assets.clone())
            .collect()
    }

    /// The transport every part of `network` reaches its nodes through:
    /// [`Swap::replace`] moves its indexer, factory, wallets and broadcaster
    /// to other nodes at once, without verifying them.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a network that is not configured.
    pub fn client(&self, network: &Network) -> anyhow::Result<&Arc<Swap>> {
        self.clients
            .get(network)
            .ok_or_else(|| unsupported().into())
    }

    /// Indexes `asset` of `address` from `birthday`, on the address's network.
    ///
    /// # Errors
    ///
    /// An unconfigured network, an asset of another network, or a chain or
    /// store failure.
    pub async fn watch(
        &self,
        address: &Address,
        asset: &Asset,
        birthday: Birthday,
    ) -> anyhow::Result<()> {
        self.service(&address.network)?
            .indexer
            .watch(address, asset, birthday)
            .await
    }

    /// Every network's indexing health.
    #[must_use]
    pub fn stats(&self) -> BTreeMap<Network, Stats> {
        self.services
            .iter()
            .map(|(network, service)| (network.clone(), service.indexer.stats()))
            .collect()
    }

    /// A new wallet of `network` and its key, kept in the wallet store.
    ///
    /// # Errors
    ///
    /// An unconfigured network, key generation, or the store.
    pub async fn generate(
        &self,
        network: &Network,
    ) -> anyhow::Result<(Arc<dyn Wallet>, SecretBytes)> {
        let service = self.service(network)?;
        let (wallet, secret) = self.factories().generate(network).await?;
        service.storage.wallets.insert(&*wallet).await?;
        Ok((wallet, secret))
    }

    /// The kept wallet of `address`, restored through its network's factory.
    ///
    /// # Errors
    ///
    /// `InvalidAddress` when no wallet of `address` is kept; an unconfigured
    /// network, or the store.
    pub async fn wallet(&self, address: &Address) -> anyhow::Result<Arc<dyn Wallet>> {
        let service = self.service(&address.network)?;
        let wallets = service.storage.wallets.find_all(&self.factories()).await?;
        wallets
            .into_iter()
            .find(|wallet| wallet.address() == address)
            .ok_or_else(|| {
                WalletError::new(WalletErrorKind::InvalidAddress, "no wallet of this address")
                    .into()
            })
    }

    /// Every kept wallet, restored through its network's factory.
    ///
    /// # Errors
    ///
    /// When the wallet store cannot read or a key does not restore.
    pub async fn wallets(&self) -> anyhow::Result<Vec<Arc<dyn Wallet>>> {
        match self.services.values().next() {
            Some(service) => service.storage.wallets.find_all(&self.factories()).await,
            None => Ok(Vec::new()),
        }
    }

    /// The canonical address of `text` in `network`.
    ///
    /// # Errors
    ///
    /// `Unsupported` for an unconfigured network; `InvalidAddress` otherwise.
    pub fn parse(&self, network: &Network, text: &str) -> anyhow::Result<Address> {
        self.service(network)?.wallets.factory.parse_address(text)
    }

    /// Submits `transaction` (its exact signed bytes) to every node of `network`.
    ///
    /// # Errors
    ///
    /// `Unsupported` for an unconfigured network; the broadcaster's failure
    /// otherwise.
    pub async fn broadcast(
        &self,
        network: &Network,
        transaction: &base::Transaction,
    ) -> Result<(), TransactionError> {
        let service = self.services.get(network).ok_or_else(|| {
            TransactionError::new(
                TransactionErrorKind::Unsupported,
                "network is not configured",
            )
        })?;
        service.wallets.broadcaster.broadcast(transaction).await
    }

    fn factories(&self) -> WalletFactories {
        let mut factories = WalletFactories::new();
        for service in self.services.values() {
            factories.set(service.wallets.clone());
        }
        factories
    }
}

fn unsupported() -> WalletError {
    WalletError::new(WalletErrorKind::Unsupported, "network is not configured")
}

impl fmt::Debug for Payments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Payments")
            .field("networks", &self.services.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Collects configuration and stores, then [`Builder::build`] connects every
/// network.
#[derive(Default)]
pub struct Builder {
    parsers: BTreeMap<Chain, Parser>,
    configs: Vec<NetworkConfig>,
    networks: Vec<Box<dyn Config>>,
    clients: BTreeMap<Network, Arc<dyn rpc::Client>>,
    storage: Option<Storage>,
}

impl Builder {
    /// Builds every [`NetworkConfig`] of `chain` as `C`. Registering a chain
    /// again replaces the earlier one.
    #[must_use]
    pub fn with_chain<C>(mut self, chain: Chain) -> Self
    where
        C: Config + for<'a> TryFrom<&'a NetworkConfig, Error = anyhow::Error> + 'static,
    {
        self.parsers
            .insert(chain, Box::new(|config| Ok(Box::new(C::try_from(config)?))));
        self
    }

    /// One more network, built by its registered chain.
    #[must_use]
    pub fn with_network_config(mut self, config: NetworkConfig) -> Self {
        self.configs.push(config);
        self
    }

    /// One more network, configured in code.
    #[must_use]
    pub fn with_network(mut self, config: impl Config + 'static) -> Self {
        self.networks.push(Box::new(config));
        self
    }

    /// Reaches `network`'s nodes through `client` instead of the chain's own
    /// [`Config::client`].
    #[must_use]
    pub fn with_client(
        mut self,
        network: impl Into<Network>,
        client: Arc<dyn rpc::Client>,
    ) -> Self {
        self.clients.insert(network.into(), client);
        self
    }

    /// Every store the networks are built over. Required.
    #[must_use]
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Builds every [`NetworkConfig`] by its chain, then each network over a
    /// [`Swap`] of its given client or the chain's own. A network that does
    /// not build (an unregistered chain, settings that do not decode, an
    /// invalid configuration or endpoint) is left out with why, see
    /// [`Payments::service`]; the others build. Nothing asks a node: each
    /// chain verifies its node at first use.
    ///
    /// # Errors
    ///
    /// No storage, a network configured twice, or a client for a network
    /// with no configuration.
    pub async fn build(self) -> anyhow::Result<Payments> {
        let storage = self.storage.context("payments needs with_storage")?;
        let mut payments = Payments {
            services: BTreeMap::new(),
            clients: BTreeMap::new(),
            failures: BTreeMap::new(),
        };

        let mut configs = Vec::new();
        for config in &self.configs {
            let network = &config.network;
            let label = format!("{}/{}", network.chain, network.name);
            let parsed = self
                .parsers
                .get(&network.chain)
                .with_context(|| {
                    let known = self.parsers.keys().collect::<Vec<_>>();
                    format!(
                        "no chain {} is registered; registered: {known:?}",
                        network.chain
                    )
                })
                .and_then(|parse| parse(config));
            match parsed {
                Ok(parsed) => configs.push(parsed),
                Err(error) => {
                    let reason = format!("{:#}", error.context(label));
                    payments.failures.insert(network.clone(), reason);
                }
            }
        }
        configs.extend(self.networks);
        let mut given = self.clients;
        for network in given.keys() {
            ensure!(
                payments.failures.contains_key(network)
                    || configs.iter().any(|config| config.network() == *network),
                "a client is given for {}/{}, which is not configured",
                network.chain,
                network.name
            );
        }

        for config in configs {
            let network = config.network();
            let label = format!("{}/{}", network.chain, network.name);
            ensure!(
                !payments.services.contains_key(&network)
                    && !payments.failures.contains_key(&network),
                "network {label} is configured twice"
            );
            let given = given.remove(&network);
            match open(&*config, given, &storage).await.context(label) {
                Ok((service, client)) => {
                    payments.services.insert(network.clone(), service);
                    payments.clients.insert(network, client);
                }
                Err(error) => {
                    payments.failures.insert(network, format!("{error:#}"));
                }
            }
        }
        Ok(payments)
    }
}

/// `config` built over a [`Swap`] of `given`, or of the chain's own client.
async fn open(
    config: &dyn Config,
    given: Option<Arc<dyn rpc::Client>>,
    storage: &Storage,
) -> anyhow::Result<(Service, Arc<Swap>)> {
    let transport = match given {
        Some(client) => client,
        None => config.client().await?,
    };
    let client = Arc::new(Swap::new(transport, Vec::new()));
    let service = config.build(storage, client.clone()).await?;
    Ok((service, client))
}

impl fmt::Debug for Builder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Builder")
            .field("chains", &self.parsers.keys().collect::<Vec<_>>())
            .field(
                "configs",
                &self.configs.iter().map(|c| &c.network).collect::<Vec<_>>(),
            )
            .field("networks", &self.networks.len())
            .field("clients", &self.clients.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}
