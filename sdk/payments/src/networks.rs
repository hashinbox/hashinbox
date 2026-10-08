//! [`Networks`]: the operational handle over every configured network —
//! its nodes and their replacement, pause, resume, the online rescan and
//! indexing health. Per-network live state that exists because the container
//! composed the chains; it names no chain.

use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use base::Network;
use indexing::{
    Condition, Control, Driver, IndexError, IndexStore, Indexers, PurgedRows, RescanError,
    SyncHealth,
};
use json_rpc::{Endpoint, Statistics};
use tokio::sync::{OwnedMutexGuard, watch};
use wallets::Wallets;

use crate::{
    Split,
    config::{ChainNodes, ErrorKind},
};

/// How long a candidate node list may take to prove its network.
const VERIFICATION: Duration = Duration::from_secs(30);

/// One configured network now.
#[derive(Clone, Debug)]
pub struct NetworkState {
    /// The network.
    pub network: Network,
    /// Whether this process composed it: indexes it and sends on it.
    pub served: bool,
    /// Whether its sends and wallet creation are paused for a rescan.
    pub rescanning: bool,
    /// Its indexing state, once it reported one.
    pub condition: Option<Condition>,
    /// Its nodes in failover order.
    pub endpoints: Vec<Endpoint>,
    /// Each node's counters while served; ordinal `i` is node `i`.
    pub statistics: Vec<Statistics>,
}

/// Why a maintenance call refused.
#[derive(Debug)]
pub enum NetworkError {
    /// The configuration names no such network.
    NotConfigured,
    /// Another replacement or rescan of this network is running.
    Busy,
    /// The network's node failed verification at startup; it is not indexed.
    NotInService,
    /// The chain refuses the candidate nodes, or they build no transport.
    Invalid(String),
    /// A candidate node did not prove it serves this network.
    Unverified(String),
    /// The caller's persistence of the verified nodes failed; the current
    /// nodes stay in use.
    NotStored(String),
    /// The rescan failed; whether the network is left paused is in it.
    Rescan(RescanError),
    /// Pausing or resuming failed.
    Index(IndexError),
    /// The maintenance task itself failed.
    Task(String),
}

impl fmt::Display for NetworkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => formatter.write_str("network is not configured"),
            Self::Busy => formatter.write_str("another change to this network is in progress"),
            Self::NotInService => formatter.write_str("the network is not in service"),
            Self::Invalid(message) | Self::Unverified(message) => formatter.write_str(message),
            Self::NotStored(message) => write!(formatter, "the nodes were not stored: {message}"),
            Self::Rescan(error) => write!(formatter, "{error}"),
            Self::Index(error) => write!(formatter, "{}", error.message),
            Self::Task(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for NetworkError {}

/// One configured network: its chain's node check, its nodes, and the one
/// change allowed on it at a time. Its nodes and its transport's counters
/// change together, so ordinal `i` of the nodes is always counter `i`.
struct Live<A> {
    chain: ChainNodes<A, Split>,
    endpoints: Mutex<Vec<Endpoint>>,
    maintenance: Arc<tokio::sync::Mutex<()>>,
}

impl<A> Live<A> {
    // Every critical section is one read or one replacement, so a panic
    // cannot leave the list torn: recovering the guard is sound.
    fn endpoints(&self) -> MutexGuard<'_, Vec<Endpoint>> {
        self.endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn maintain(&self) -> Result<OwnedMutexGuard<()>, NetworkError> {
        self.maintenance
            .clone()
            .try_lock_owned()
            .map_err(|_| NetworkError::Busy)
    }
}

/// The operational handle; cheap to clone, every clone the same networks.
pub struct Networks<A: Ord> {
    live: Arc<[Arc<Live<A>>]>,
    wallets: Wallets<A>,
    index: Arc<Indexers>,
    store: Arc<dyn IndexStore>,
    control: Control,
    health: watch::Receiver<SyncHealth>,
}

impl<A: Clone + Ord> Clone for Networks<A> {
    fn clone(&self) -> Self {
        Self {
            live: self.live.clone(),
            wallets: self.wallets.clone(),
            index: self.index.clone(),
            store: self.store.clone(),
            control: self.control.clone(),
            health: self.health.clone(),
        }
    }
}

impl<A> Networks<A>
where
    A: Clone + Ord + Send + Sync + 'static,
{
    pub(crate) fn new(
        chains: Vec<ChainNodes<A, Split>>,
        mut endpoints: BTreeMap<Network, Vec<Endpoint>>,
        wallets: Wallets<A>,
        index: Arc<Indexers>,
        store: Arc<dyn IndexStore>,
        driver: &Driver,
    ) -> Self {
        let live = chains
            .into_iter()
            .map(|chain| {
                let nodes = endpoints.remove(chain.network()).unwrap_or_default();
                Arc::new(Live {
                    chain,
                    endpoints: Mutex::new(nodes),
                    maintenance: Arc::new(tokio::sync::Mutex::new(())),
                })
            })
            .collect();
        Self {
            live,
            wallets,
            index,
            store,
            control: driver.control(),
            health: driver.health(),
        }
    }

    /// Every configured network, composed or skipped, in configuration order.
    #[must_use]
    pub fn list(&self) -> Vec<NetworkState> {
        self.live.iter().map(|live| self.state(live)).collect()
    }

    /// One configured network now.
    ///
    /// # Errors
    ///
    /// [`NetworkError::NotConfigured`].
    pub fn get(&self, network: &Network) -> Result<NetworkState, NetworkError> {
        Ok(self.state(self.find(network)?))
    }

    /// Every network's indexing health now.
    #[must_use]
    pub fn health(&self) -> SyncHealth {
        self.health.borrow().clone()
    }

    /// A receiver of every health change.
    #[must_use]
    pub fn health_receiver(&self) -> watch::Receiver<SyncHealth> {
        self.health.clone()
    }

    /// Stops reporting ready, the first step of a shutdown.
    pub fn reset_health(&self) {
        self.control.reset_health();
    }

    /// Replaces `network`'s nodes: the chain's own startup check over
    /// `endpoints`, then `persist` (the caller's storage of the list; pass
    /// `async { Ok(()) }` for none), then every later call goes to the new
    /// transport. A node that fails the check, or a failed `persist`, leaves
    /// the current nodes in use. Persist and install run in a task of their
    /// own, so a caller that hangs up cannot leave them apart. A network not
    /// in service only records the nodes, used from the next start.
    ///
    /// # Errors
    ///
    /// [`NetworkError::NotConfigured`], [`NetworkError::Busy`],
    /// [`NetworkError::Invalid`], [`NetworkError::Unverified`],
    /// [`NetworkError::NotStored`], [`NetworkError::Task`].
    pub async fn replace<P>(
        &self,
        network: &Network,
        endpoints: Vec<Endpoint>,
        persist: P,
    ) -> Result<NetworkState, NetworkError>
    where
        P: Future<Output = Result<(), String>> + Send + 'static,
    {
        let live = self.find(network)?.clone();
        let _held = live.maintain()?;
        let check = live.chain.verify(endpoints.clone());
        let Ok(verified) = tokio::time::timeout(VERIFICATION, check).await else {
            return Err(NetworkError::Unverified(format!(
                "the node did not prove its network within {} seconds",
                VERIFICATION.as_secs()
            )));
        };
        let (transport, statistics) = verified.map_err(|error| {
            if error.kind == ErrorKind::Unverified {
                NetworkError::Unverified(error.to_string())
            } else {
                NetworkError::Invalid(error.to_string())
            }
        })?;
        let installed = live.clone();
        tokio::spawn(async move {
            persist.await.map_err(NetworkError::NotStored)?;
            let mut current = installed.endpoints();
            if let Some(swap) = installed.chain.transport() {
                swap.replace(transport, statistics);
            }
            *current = endpoints;
            Ok(())
        })
        .await
        .map_err(|error| NetworkError::Task(error.to_string()))??;
        Ok(self.state(&live))
    }

    /// Pauses `network`'s indexing after its current pass.
    ///
    /// # Errors
    ///
    /// The driver's refusal.
    pub async fn pause(&self, network: &Network) -> Result<(), NetworkError> {
        self.control
            .pause(network)
            .await
            .map_err(NetworkError::Index)
    }

    /// Resumes `network`'s indexing.
    ///
    /// # Errors
    ///
    /// The driver's refusal.
    pub fn resume(&self, network: &Network) -> Result<(), NetworkError> {
        self.control.resume(network).map_err(NetworkError::Index)
    }

    /// The online rescan, as one action: new wallets and sends on the network
    /// are refused and the running ones drained, its indexing paused, its
    /// indexed facts purged, and indexing resumed from every watch's birthday.
    /// Runs in its own task, so the window closes and reopens whole even if
    /// the caller hangs up. Every other network keeps syncing.
    ///
    /// # Errors
    ///
    /// [`NetworkError::NotConfigured`], [`NetworkError::NotInService`],
    /// [`NetworkError::Busy`], [`NetworkError::Rescan`],
    /// [`NetworkError::Task`].
    pub async fn rescan(&self, network: &Network) -> Result<PurgedRows, NetworkError> {
        let live = self.find(network)?;
        if live.chain.transport().is_none() {
            return Err(NetworkError::NotInService);
        }
        let held = live.maintain()?;
        let indexer = self
            .index
            .units()
            .iter()
            .find(|unit| unit.network() == network)
            .map(indexing::Route::indexer)
            .ok_or(NetworkError::NotInService)?;
        let (wallets, control, store) = (
            self.wallets.clone(),
            self.control.clone(),
            self.store.clone(),
        );
        tokio::spawn(async move {
            let _held = held;
            let paused = wallets.pause(indexer.network());
            paused.drained().await;
            control.rescan(indexer.as_ref(), store.as_ref()).await
        })
        .await
        .map_err(|error| NetworkError::Task(error.to_string()))?
        .map_err(NetworkError::Rescan)
    }

    fn find(&self, network: &Network) -> Result<&Arc<Live<A>>, NetworkError> {
        self.live
            .iter()
            .find(|live| live.chain.network() == network)
            .ok_or(NetworkError::NotConfigured)
    }

    fn state(&self, live: &Live<A>) -> NetworkState {
        let network = live.chain.network().clone();
        let endpoints = live.endpoints().clone();
        let statistics = live
            .chain
            .transport()
            .map(|swap| swap.statistics())
            .unwrap_or_default();
        let condition = self
            .health
            .borrow()
            .networks()
            .iter()
            .find(|row| row.status.network == network)
            .map(|row| row.state);
        NetworkState {
            served: live.chain.transport().is_some(),
            rescanning: self.wallets.paused(&network),
            condition,
            endpoints,
            statistics,
            network,
        }
    }
}
