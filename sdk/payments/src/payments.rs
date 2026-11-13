//! [`Payments`]: every configured network built over the application's
//! stores, and its [`Builder`].

use std::{collections::BTreeMap, fmt, sync::Arc};

use anyhow::{Context as _, ensure};
use base::{
    Address, Asset, Assets, Birthday, Chain, Network, NetworkConfig, NodeInfo, Service,
    ServiceFactory, Stats, Storage, TransactionError, TransactionErrorKind, Wallet, WalletError,
    WalletErrorKind, WalletFactories,
};
use crypto::SecretBytes;
use rpc::{Retry, Swap};

/// A network's nodes: its endpoints in failover order, clustered by tip, and
/// the methods spread round robin over some of them instead
/// ([`rpc::Router`]). Without routes there is no router.
///
/// ```
/// # fn nodes() -> anyhow::Result<payments::Nodes> {
/// let endpoints = vec![rpc::Endpoint::new("https://a.invalid/"), rpc::Endpoint::new("https://b.invalid/")];
/// payments::Nodes::new(endpoints).route("getBlock", [0, 1])
/// # }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nodes {
    endpoints: Vec<rpc::Endpoint>,
    routes: Vec<(String, Vec<usize>)>,
}

impl Nodes {
    /// `endpoints` in failover order, every method to their cluster.
    #[must_use]
    pub fn new(endpoints: Vec<rpc::Endpoint>) -> Self {
        Self {
            endpoints,
            routes: Vec::new(),
        }
    }

    /// The same, `method` round robin over the endpoints at `positions`.
    ///
    /// # Errors
    ///
    /// No positions, or one past the endpoints.
    pub fn route(
        mut self,
        method: impl Into<String>,
        positions: impl IntoIterator<Item = usize>,
    ) -> anyhow::Result<Self> {
        let method = method.into();
        let positions: Vec<usize> = positions.into_iter().collect();
        ensure!(!positions.is_empty(), "route {method} names no endpoint");
        ensure!(
            positions
                .iter()
                .all(|&position| position < self.endpoints.len()),
            "route {method} names no such endpoint"
        );
        self.routes.push((method, positions));
        Ok(self)
    }

    /// One transport over every node, primary chosen by `config`'s tip, each
    /// routed method round robin over its nodes instead, transport failures
    /// retried; and each node's counters, which a node shares between the
    /// cluster and its routes. Asks no node.
    fn client(
        &self,
        config: &Arc<dyn ServiceFactory>,
    ) -> anyhow::Result<(Arc<dyn rpc::Client>, Vec<rpc::Statistics>)> {
        let mut nodes: Vec<Arc<dyn rpc::Client>> = Vec::new();
        let mut statistics = Vec::new();
        for endpoint in &self.endpoints {
            // No sink yet: nothing reads RPC failure events.
            let node = rpc::node(endpoint, Default::default())?;
            statistics.push(node.statistics());
            nodes.push(Arc::new(node));
        }
        let cluster = config.info(nodes.clone())?;
        if self.routes.is_empty() {
            return Ok((Arc::new(Retry::new(cluster)), statistics));
        }
        let mut router = rpc::Router::new(cluster);
        for (method, positions) in &self.routes {
            let clients = positions.iter().map(|&position| nodes[position].clone());
            router = router.route([method.as_str()], clients.collect())?;
        }
        Ok((Arc::new(Retry::new(router)), statistics))
    }
}

/// Builds a registered chain's configuration from a [`NetworkConfig`].
type Parser = Box<dyn Fn(&NetworkConfig) -> anyhow::Result<Box<dyn ServiceFactory>> + Send + Sync>;

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
    /// Each built network's configuration, for [`Payments::info`].
    configs: BTreeMap<Network, Arc<dyn ServiceFactory>>,
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

    /// The transport every part of `network` reaches its nodes through;
    /// [`Self::connect`] moves it to other nodes.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a network that is not configured.
    pub fn client(&self, network: &Network) -> anyhow::Result<&Arc<Swap>> {
        self.clients
            .get(network)
            .ok_or_else(|| unsupported().into())
    }

    /// What the node at `endpoint` serves, asked as `network`'s chain asks
    /// it: once, when a node is added.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a network that is not configured; an invalid
    /// endpoint, a node failure, or a node of another chain.
    pub async fn info(
        &self,
        network: &Network,
        endpoint: &rpc::Endpoint,
    ) -> anyhow::Result<NodeInfo> {
        let config = self.configs.get(network).ok_or_else(unsupported)?;
        let node = rpc::node(endpoint, Default::default())?;
        config.info(vec![Arc::new(node)])?.info().await
    }

    /// Sends every later call of `network` (indexer, factory, wallets,
    /// broadcaster) to `nodes`; asks no node.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a network that is not configured; no or an invalid
    /// endpoint.
    pub fn connect(&self, network: &Network, nodes: &Nodes) -> anyhow::Result<()> {
        let config = self.configs.get(network).ok_or_else(unsupported)?;
        let (transport, statistics) = nodes.client(config)?;
        self.client(network)?.replace(transport, statistics);
        Ok(())
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
    networks: Vec<(Box<dyn ServiceFactory>, Vec<rpc::Endpoint>)>,
    clients: BTreeMap<Network, Arc<dyn rpc::Client>>,
    nodes: BTreeMap<Network, Nodes>,
    storage: Option<Storage>,
}

impl Builder {
    /// Builds every [`NetworkConfig`] of `chain` as `C`. Registering a chain
    /// again replaces the earlier one.
    #[must_use]
    pub fn with_chain<C>(mut self, chain: Chain) -> Self
    where
        C: ServiceFactory + for<'a> TryFrom<&'a NetworkConfig, Error = anyhow::Error> + 'static,
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

    /// One more network, configured in code, over `endpoints` in failover
    /// order.
    #[must_use]
    pub fn with_network(
        mut self,
        config: impl ServiceFactory + 'static,
        endpoints: Vec<rpc::Endpoint>,
    ) -> Self {
        self.networks.push((Box::new(config), endpoints));
        self
    }

    /// Reaches `network`'s nodes through `client` instead of a cluster of
    /// its endpoints.
    #[must_use]
    pub fn with_client(
        mut self,
        network: impl Into<Network>,
        client: Arc<dyn rpc::Client>,
    ) -> Self {
        self.clients.insert(network.into(), client);
        self
    }

    /// Reaches `network` through `nodes` instead of its configured
    /// endpoints; ignored where a client is given.
    #[must_use]
    pub fn with_nodes(mut self, network: impl Into<Network>, nodes: Nodes) -> Self {
        self.nodes.insert(network.into(), nodes);
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
    /// [`Payments::service`]; the others build. Nothing asks a node.
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
            configs: BTreeMap::new(),
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
                Ok(parsed) => configs.push((parsed, config.endpoints.clone())),
                Err(error) => {
                    let reason = format!("{:#}", error.context(label));
                    payments.failures.insert(network.clone(), reason);
                }
            }
        }
        configs.extend(self.networks);
        let mut given = self.clients;
        let mut nodes = self.nodes;
        for network in given.keys() {
            ensure!(
                payments.failures.contains_key(network)
                    || configs
                        .iter()
                        .any(|(config, _)| config.network() == *network),
                "a client is given for {}/{}, which is not configured",
                network.chain,
                network.name
            );
        }

        for (config, endpoints) in configs {
            let config: Arc<dyn ServiceFactory> = Arc::from(config);
            let network = config.network();
            let label = format!("{}/{}", network.chain, network.name);
            ensure!(
                !payments.services.contains_key(&network)
                    && !payments.failures.contains_key(&network),
                "network {label} is configured twice"
            );
            let given = given.remove(&network);
            let nodes = nodes
                .remove(&network)
                .unwrap_or_else(|| Nodes::new(endpoints));
            match open(&config, &nodes, given, &storage).await.context(label) {
                Ok((service, client)) => {
                    payments.services.insert(network.clone(), service);
                    payments.clients.insert(network.clone(), client);
                    payments.configs.insert(network, config);
                }
                Err(error) => {
                    payments.failures.insert(network, format!("{error:#}"));
                }
            }
        }
        Ok(payments)
    }
}

/// `config` built over a [`Swap`] of `given`, or of `nodes`.
async fn open(
    config: &Arc<dyn ServiceFactory>,
    nodes: &Nodes,
    given: Option<Arc<dyn rpc::Client>>,
    storage: &Storage,
) -> anyhow::Result<(Service, Arc<Swap>)> {
    let (transport, statistics) = match given {
        Some(client) => (client, Vec::new()),
        None => nodes.client(config)?,
    };
    let client = Arc::new(Swap::new(transport, statistics));
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
