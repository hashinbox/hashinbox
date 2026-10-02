//! Application lifecycle: independent network workers over one repository and settings table.
mod access;
mod connection;
mod network;
mod recovery;
mod status;
pub(crate) use access::Access;
use network::ActiveNetwork;
pub(crate) use status::Status;

use crate::{
    api::{Application, Payments, Signers},
    config::{AnyError, Config, ConfiguredWallet},
    lock::Lock as _,
    registry::Registry,
    settings::Settings,
    storage::Repository,
};
use base::Network;
use indexing::{Index, Router, SyncHealth};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, task::JoinSet};

pub(crate) struct Runtime {
    pub(crate) settings: Arc<Settings>,
    pub(crate) repository: Repository,
    pub(crate) status: Status,
    /// The composition every request reads: one immutable snapshot, swapped
    /// whole, so a request sees the old one or the new one and never a mix.
    published: watch::Sender<Arc<Payments>>,
    registry: Arc<Registry>,
    signers: Arc<Signers>,
    imports: Vec<ConfiguredWallet>,
    active: Mutex<BTreeMap<Network, Arc<ActiveNetwork>>>,
    health: watch::Sender<SyncHealth>,
}

impl Runtime {
    /// Opens storage and settings, and publishes the first snapshot: every
    /// configured network inactive, health starting. The listener binds after
    /// this returns, so no request ever precedes a publication.
    pub(crate) async fn new(config: Config) -> Result<Arc<Self>, AnyError> {
        let path = config.storage.resolved_path()?;
        let status = Status::new();
        let repository = config
            .storage
            .open()
            .await?
            .repository()
            .with_logger(status.logger.clone());
        let registry = Registry::open(&path).await?;
        let seed = Settings::seed(config.chains)?;
        let settings = Settings::open(&path, seed).await?;
        let signers = Arc::new(Signers::new());
        let networks = settings.networks();
        let (health, receiver) = watch::channel(SyncHealth::starting(&networks));
        let first = Payments {
            wallets: Arc::new(wallets::Wallets::new()),
            units: Arc::new(base::Units::default()),
            signers: signers.clone(),
            registry: registry.clone(),
            index: Arc::new(Index::of(vec![])?),
            history: Arc::new(Router::new(Vec::new())?),
            balance: Arc::new(Router::new(Vec::new())?),
            health: receiver,
            rpc: BTreeMap::new(),
            inactive: networks.iter().cloned().collect(),
            logs: status.logs.clone(),
            logger: status.logger.clone(),
            control: None,
            access: BTreeMap::new(),
        };
        Ok(Arc::new(Self {
            settings,
            repository,
            status,
            published: watch::channel(Arc::new(first)).0,
            registry,
            signers,
            imports: config.wallets,
            active: Mutex::new(BTreeMap::new()),
            health,
        }))
    }

    pub(crate) fn stop_admission(&self) {
        for active in self.active.hold().values() {
            active.access.block();
        }
    }

    pub(crate) fn application(self: &Arc<Self>) -> Application {
        Application {
            payments: self.published.subscribe(),
            runtime: Some(self.clone()),
        }
    }

    pub(crate) fn active(&self, network: &Network) -> Option<Arc<ActiveNetwork>> {
        self.active.hold().get(network).cloned()
    }

    pub(crate) async fn run(self: Arc<Self>, shutdown: watch::Receiver<bool>) {
        let mut tasks = JoinSet::new();
        let mut started = BTreeSet::new();
        let mut signals = Signals::new(shutdown, self.settings.changes());
        while !signals.stopped() {
            self.spawn(&mut tasks, &mut started, &signals.shutdown);
            self.publish_health();
            tokio::select! {
                () = signals.wait(Duration::from_secs(1)) => {},
                Some(result) = tasks.join_next() => self.joined(&result),
            }
        }
        while tasks.join_next().await.is_some() {}
        self.health
            .send_replace(SyncHealth::starting(&self.settings.networks()));
    }

    /// Starts one control task for each configured network not yet started.
    fn spawn(
        self: &Arc<Self>,
        tasks: &mut JoinSet<()>,
        started: &mut BTreeSet<Network>,
        shutdown: &watch::Receiver<bool>,
    ) {
        let networks = self.settings.networks();
        for network in networks.iter().filter(|n| started.insert((*n).clone())) {
            tasks.spawn(self.clone().drive(network.clone(), shutdown.clone()));
        }
    }

    fn joined(&self, result: &Result<(), tokio::task::JoinError>) {
        if result.is_err() {
            self.status.logger.event(
                logging::Level::Error,
                "runtime.failed",
                "Network control task stopped",
                &[],
            );
        }
    }

    async fn drive(self: Arc<Self>, network: Network, shutdown: watch::Receiver<bool>) {
        let mut signals = Signals::new(shutdown, self.settings.changes());
        while !signals.stopped() {
            if let Some((revision, desired)) = self.settings.desired(&network) {
                let result = self
                    .reconcile(&network, revision, desired, &mut signals.shutdown)
                    .await;
                self.status.failed(&network, result.is_err());
            }
            self.publish_health();
            signals.wait(Duration::from_secs(5)).await;
        }
        if let Some(active) = self.active(&network) {
            active.stop().await;
        }
    }

    /// Swaps in a snapshot of every active network's composition.
    fn publish(&self) -> Result<(), AnyError> {
        let active = self.active.hold().clone();
        let mut wallets = wallets::Wallets::new();
        let mut units = base::Units::default();
        let mut indexes = Vec::new();
        let mut access = BTreeMap::new();
        let mut rpc = BTreeMap::new();
        let mut history = Vec::new();
        let mut balance = Vec::new();
        for (network, active) in &active {
            wallets.extend(&active.wallets)?;
            units.extend(active.units.clone());
            indexes.push(active.indexer.clone());
            access.insert(network.clone(), active.access.clone());
            rpc.insert(network.clone(), active.statistics());
            history.push((network.clone(), active.history.clone()));
            balance.push((network.clone(), active.balance.clone()));
        }
        let inactive = self
            .settings
            .networks()
            .iter()
            .filter(|network| !active.contains_key(network))
            .cloned()
            .collect();
        let previous = self.published.borrow().clone();
        let next = Payments {
            wallets: Arc::new(wallets),
            units: Arc::new(units),
            index: Arc::new(Index::of(indexes)?),
            history: Arc::new(Router::new(history)?),
            balance: Arc::new(Router::new(balance)?),
            access,
            rpc,
            inactive,
            ..(*previous).clone()
        };
        self.published.send_replace(Arc::new(next));
        Ok(())
    }

    fn publish_health(&self) {
        let mut health = SyncHealth::starting(&self.settings.networks());
        for active in self.active.hold().values() {
            for network in active.health.borrow().networks() {
                health.apply(network);
            }
        }
        self.health.send_replace(health);
    }
}

/// What a control loop waits on between passes: shutdown, a settings change,
/// or its own period elapsing.
struct Signals {
    shutdown: watch::Receiver<bool>,
    changes: watch::Receiver<Arc<Vec<Network>>>,
}

impl Signals {
    fn new(shutdown: watch::Receiver<bool>, changes: watch::Receiver<Arc<Vec<Network>>>) -> Self {
        Self { shutdown, changes }
    }

    /// Whether shutdown was requested, or its sender is gone.
    fn stopped(&self) -> bool {
        *self.shutdown.borrow() || self.shutdown.has_changed().is_err()
    }

    /// Waits for the first signal, or for `period` to elapse.
    async fn wait(&mut self, period: Duration) {
        tokio::select! {
            _ = self.shutdown.changed() => {},
            _ = self.changes.changed() => {},
            () = tokio::time::sleep(period) => {},
        }
    }
}
