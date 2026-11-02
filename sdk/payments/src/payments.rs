//! [`Payments`]: every configured network built over the application's
//! stores, and its [`Builder`].

use std::{collections::BTreeMap, fmt, sync::Arc};

use anyhow::{Context as _, ensure};
use base::{
    Address, Asset, Assets, Birthday, Config, Network, Service, Stats, Storage, TransactionError,
    TransactionErrorKind, Wallet, WalletError, WalletErrorKind, WalletFactories,
};
use crypto::SecretBytes;
use rpc::Swap;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::Document;

/// Builds a registered chain's configuration from its section.
type Parser = Box<dyn Fn(Value) -> anyhow::Result<Box<dyn Config>> + Send + Sync>;

/// Every configured network's [`Service`], and the swappable transport it
/// reaches its nodes through; every call is keyed by network.
///
/// It names no chain: the application registers each chain's configuration
/// with [`Builder::with_chain`], or hands configurations in with
/// [`Builder::with_network`].
pub struct Payments {
    services: BTreeMap<Network, Service>,
    clients: BTreeMap<Network, Arc<Swap>>,
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
    /// `Unsupported` for a network that is not configured.
    pub fn service(&self, network: &Network) -> anyhow::Result<&Service> {
        self.services
            .get(network)
            .ok_or_else(|| unsupported().into())
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
    parsers: BTreeMap<String, Parser>,
    document: Option<Document>,
    networks: Vec<Box<dyn Config>>,
    clients: BTreeMap<Network, Arc<dyn rpc::Client>>,
    storage: Option<Storage>,
}

impl Builder {
    /// Reads the document's sections whose `chain` is `name` as `C`.
    /// Registering a name again replaces the earlier chain.
    #[must_use]
    pub fn with_chain<C: Config + DeserializeOwned + 'static>(
        mut self,
        name: impl Into<String>,
    ) -> Self {
        self.parsers.insert(
            name.into(),
            Box::new(|section| Ok(Box::new(serde_json::from_value::<C>(section)?))),
        );
        self
    }

    /// The configuration document: its networks, read by the registered
    /// chains.
    #[must_use]
    pub fn with_config(mut self, document: Document) -> Self {
        self.document = Some(document);
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

    /// Reads every section, then builds each network over a [`Swap`] of its
    /// given client or the chain's own.
    ///
    /// # Errors
    ///
    /// No storage, a section of an unregistered chain or that does not
    /// decode, a network configured twice, a client for a network with no
    /// configuration, or a network whose configuration is invalid or whose
    /// nodes do not connect.
    pub async fn build(self) -> anyhow::Result<Payments> {
        let storage = self.storage.context("payments needs with_storage")?;

        let mut configs = Vec::new();
        for (chain, section) in self
            .document
            .as_ref()
            .map_or(Ok(Vec::new()), Document::sections)?
        {
            let parse = self.parsers.get(&chain).with_context(|| {
                let known = self.parsers.keys().cloned().collect::<Vec<_>>();
                format!("no chain {chain:?} is registered; registered: {known:?}")
            })?;
            configs.push(parse(section).with_context(|| chain.clone())?);
        }
        configs.extend(self.networks);
        let mut given = self.clients;
        for network in given.keys() {
            ensure!(
                configs.iter().any(|config| config.network() == *network),
                "a client is given for {}/{}, which is not configured",
                network.chain,
                network.name
            );
        }

        let mut payments = Payments {
            services: BTreeMap::new(),
            clients: BTreeMap::new(),
        };
        for config in configs {
            let network = config.network();
            let label = format!("{}/{}", network.chain, network.name);
            ensure!(
                !payments.services.contains_key(&network),
                "network {label} is configured twice"
            );
            let transport = match given.remove(&network) {
                Some(client) => client,
                None => config.client().await.with_context(|| label.clone())?,
            };
            let client = Arc::new(Swap::new(transport, Vec::new()));
            let service = config
                .build(&storage, client.clone())
                .await
                .with_context(|| label.clone())?;
            payments.services.insert(network.clone(), service);
            payments.clients.insert(network, client);
        }
        Ok(payments)
    }
}

impl fmt::Debug for Builder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Builder")
            .field("chains", &self.parsers.keys().collect::<Vec<_>>())
            .field("document", &self.document)
            .field("networks", &self.networks.len())
            .field("clients", &self.clients.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}
