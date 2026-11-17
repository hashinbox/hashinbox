//! [`Container`]: every registered network, its [`Service`] and its
//! [`Transport`], keyed by network; [`Simple`], a transport made upfront.

use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};

use crypto::SecretBytes;

use crate::{
    Address, Addressed, Assets, Network, NetworkConfig, Networked, Service, ServiceFactory, Stats,
    Storage, Transaction, TransactionError, TransactionErrorKind, Wallet, WalletError,
    WalletErrorKind, WalletFactories,
};

/// How a network gets its one client.
pub trait Transport: Send + Sync {
    /// The client every part of the network's service reaches its nodes
    /// through.
    fn client(&self) -> Arc<dyn rpc::Client>;
}

/// A client made upfront: tests, custom stacks. No nodes, no state.
pub struct Simple(pub Arc<dyn rpc::Client>);

impl Transport for Simple {
    fn client(&self) -> Arc<dyn rpc::Client> {
        self.0.clone()
    }
}

/// Every registered network: its service (or why it did not build) and its
/// transport. It names no chain; every call is keyed by network.
pub struct Container<T: Transport> {
    storage: Storage,
    services: RwLock<BTreeMap<Network, Result<Arc<Service>, String>>>,
    transports: RwLock<BTreeMap<Network, Arc<T>>>,
}

impl<T: Transport> Container<T> {
    /// No networks yet, every service to be built over `storage`.
    #[must_use]
    pub fn new(storage: Storage) -> Self {
        Self {
            storage,
            services: RwLock::default(),
            transports: RwLock::default(),
        }
    }

    /// `config`'s network: its factory (`F::new`), its service over the
    /// storage and `transport`'s client, and its transport. A failure of
    /// either is kept with why ([`Self::service`]). Asks no node;
    /// registering a network again replaces it.
    pub fn register<F: ServiceFactory>(&self, config: NetworkConfig, transport: T) -> &Self {
        let network = config.network.clone();
        let label = format!("{}/{}", network.chain, network.name);
        let service = F::new(&config)
            .and_then(|factory| factory.service(&self.storage, transport.client()))
            .map(Arc::new)
            .map_err(|error| format!("{:#}", error.context(label)));
        write(&self.services).insert(network.clone(), service);
        write(&self.transports).insert(network, Arc::new(transport));
        self
    }

    /// The service of the network `on` lives on.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a network that is not registered, or that did not
    /// build (with why).
    pub fn service(&self, on: &impl Networked) -> anyhow::Result<Arc<Service>> {
        let services = read(&self.services);
        let Some(service) = services.get(on.network()) else {
            return Err(unsupported().into());
        };
        service.clone().map_err(|reason| {
            WalletError::new(
                WalletErrorKind::Unsupported,
                format!("network did not build: {reason}"),
            )
            .into()
        })
    }

    /// The transport of the network `on` lives on, built or not.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a network that is not registered.
    pub fn transport(&self, on: &impl Networked) -> anyhow::Result<Arc<T>> {
        read(&self.transports)
            .get(on.network())
            .cloned()
            .ok_or_else(|| unsupported().into())
    }

    /// The networks that built, in order.
    #[must_use]
    pub fn networks(&self) -> Vec<Network> {
        self.built()
            .into_iter()
            .map(|(network, _)| network)
            .collect()
    }

    /// Every built network's native currency and configured tokens.
    #[must_use]
    pub fn assets(&self) -> Assets {
        self.built()
            .into_iter()
            .flat_map(|(_, service)| service.assets.clone())
            .collect()
    }

    /// Every built network's indexing health.
    #[must_use]
    pub fn stats(&self) -> BTreeMap<Network, Stats> {
        self.built()
            .into_iter()
            .map(|(network, service)| (network, service.indexer.stats()))
            .collect()
    }

    /// A new wallet of `network` and its key, kept in the wallet store.
    ///
    /// # Errors
    ///
    /// An unregistered network, key generation, or the store.
    pub async fn generate(
        &self,
        network: &Network,
    ) -> anyhow::Result<(Arc<dyn Wallet>, SecretBytes)> {
        self.service(network)?;
        let (wallet, secret) = self.factories().generate(network).await?;
        self.storage.wallets.insert(&*wallet).await?;
        Ok((wallet, secret))
    }

    /// The kept wallet of `address`, restored through its network's factory.
    ///
    /// # Errors
    ///
    /// `InvalidAddress` when no wallet of `address` is kept; an unregistered
    /// network, or the store.
    pub async fn wallet(&self, address: &Address) -> anyhow::Result<Arc<dyn Wallet>> {
        self.service(address)?;
        let wallets = self.wallets().await?;
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
        self.storage.wallets.find_all(&self.factories()).await
    }

    /// `address` in its chain's canonical form: what a stored `Address` holds.
    ///
    /// # Errors
    ///
    /// `Unsupported` for an unregistered network; `InvalidAddress` otherwise.
    pub fn parse(&self, address: &impl Addressed) -> anyhow::Result<Address> {
        let service = self.service(address.network())?;
        service
            .wallets
            .factory
            .parse_address(&address.address().value)
    }

    /// Submits `transaction` (its exact signed bytes) to every node of `network`.
    ///
    /// # Errors
    ///
    /// `Unsupported` for an unregistered network; the broadcaster's failure
    /// otherwise.
    pub async fn broadcast(
        &self,
        network: &Network,
        transaction: &Transaction,
    ) -> Result<(), TransactionError> {
        let service = self.service(network).map_err(|_| {
            TransactionError::new(
                TransactionErrorKind::Unsupported,
                "network is not configured",
            )
        })?;
        service.wallets.broadcaster.broadcast(transaction).await
    }

    /// Every built network's service, in order.
    fn built(&self) -> Vec<(Network, Arc<Service>)> {
        read(&self.services)
            .iter()
            .filter_map(|(network, service)| Some((network.clone(), service.clone().ok()?)))
            .collect()
    }

    fn factories(&self) -> WalletFactories {
        let mut factories = WalletFactories::new();
        for (_, service) in self.built() {
            factories.set(service.wallets.clone());
        }
        factories
    }
}

fn unsupported() -> WalletError {
    WalletError::new(WalletErrorKind::Unsupported, "network is not configured")
}

// Each critical section is one insert or one read, so a panic cannot leave a
// map torn: recovering the guard is sound.
fn read<V>(lock: &RwLock<V>) -> std::sync::RwLockReadGuard<'_, V> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<V>(lock: &RwLock<V>) -> std::sync::RwLockWriteGuard<'_, V> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
