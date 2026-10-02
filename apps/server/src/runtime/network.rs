//! One network's connections, sender ownership and maintenance lifecycle.
use super::{Access, Runtime, connection::Connection};
use crate::{
    api::AssetTag,
    config::{AnyError, Environment},
    lock::Lock as _,
    settings::{self, Requested},
};
use base::Network;
use std::sync::{Arc, Mutex};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) struct ActiveNetwork {
    pub(crate) indexer: Arc<dyn indexing::Indexing>,
    pub(crate) wallets: wallets::Wallets<AssetTag>,
    pub(crate) units: base::Units,
    pub(crate) history: Arc<dyn indexing::History>,
    pub(crate) balance: Arc<dyn indexing::Balances>,
    pub(crate) access: Arc<Access>,
    pub(crate) health: watch::Receiver<indexing::SyncHealth>,
    pub(super) control: indexing::SyncControl,
    connection: Arc<Connection>,
    pub(super) submissions: Arc<wallets::Submissions>,
    shutdown: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    applied: Mutex<ConnectionState>,
}

struct ConnectionState {
    revision: u64,
    resume: u64,
    node: String,
    statistics: json_rpc::Statistics,
    nodes: Vec<settings::Node>,
}

impl ConnectionState {
    fn new(revision: u64, desired: &settings::Network, candidate: &NodeConnection) -> Self {
        Self {
            revision,
            resume: desired.resume,
            node: candidate.node.clone(),
            statistics: candidate.statistics.clone(),
            nodes: desired.nodes.clone(),
        }
    }
}

struct NodeConnection {
    connection: Arc<Connection>,
    verified: Vec<Box<dyn wallets::Connection<AssetTag, crate::storage::Repository>>>,
    statistics: json_rpc::Statistics,
    node: String,
}

impl NodeConnection {
    async fn verify(
        runtime: &Runtime,
        network: &Network,
        desired: &settings::Network,
        node: &settings::Node,
    ) -> Result<Self, AnyError> {
        let logger = runtime
            .status
            .logger
            .with("chain", network.chain.to_string())
            .with("network", network.name.clone())
            .with("component", "rpc")
            .with("node_id", node.id.clone());
        let http = node.parsed()?.connect(&Environment)?.with_logger(logger);
        let statistics = http.statistics();
        runtime.status.track(network, &node.id, statistics.clone());
        let repeat: sdk_config::Repetition = desired
            .parameters
            .pointer("/rpc/repeat")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()?
            .unwrap_or_default();
        let connection = Arc::new(Connection::new(repeat.wrap(http)));
        let configured = desired
            .chains(&network.chain.to_string(), Some(node))?
            .with_logger(runtime.status.logger.clone())
            .with_transport(connection.clone())?;
        let verified = tokio::time::timeout(
            std::time::Duration::from_secs(45),
            configured.connect(&Environment),
        )
        .await??;
        Ok(Self {
            connection,
            verified,
            statistics,
            node: node.id.clone(),
        })
    }
}

impl ActiveNetwork {
    pub(crate) fn revision(&self) -> u64 {
        self.applied.hold().revision
    }
    pub(crate) fn node(&self) -> String {
        self.applied.hold().node.clone()
    }
    pub(crate) fn statistics(&self) -> json_rpc::Statistics {
        self.applied.hold().statistics.clone()
    }
    pub(crate) async fn stop(&self) {
        self.access.block();
        let _admission = self.access.idle().await;
        self.submissions.close();
        let _ = self.submissions.drain().await;
        self.shutdown.send_replace(true);
        let task = self.task.hold().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    async fn replace(
        &self,
        runtime: &Runtime,
        network: &Network,
        revision: u64,
        desired: &settings::Network,
        candidate: NodeConnection,
    ) -> Result<(), AnyError> {
        let _admission = self.access.idle().await?;
        let result = async move {
            if !self.submissions.idle()? {
                return Err("waiting for background submissions".into());
            }
            self.control.pause(network).await?;
            if !runtime.settings.current(network, revision) {
                return Err("configuration changed during verification".into());
            }
            self.connection.replace(candidate.connection.snapshot());
            *self.applied.hold() = ConnectionState::new(revision, desired, &candidate);
            if desired.indexing == Requested::Running && desired.rebuild.is_none() {
                self.control.resume(network)?;
            }
            Ok(())
        }
        .await;
        if desired.rebuild.is_none() {
            self.access.open();
        }
        result
    }

    fn needs_connection(&self, desired: &settings::Network) -> bool {
        let applied = self.applied.hold();
        applied.nodes != desired.nodes || applied.statistics.snapshot().consecutive_failures > 0
    }

    async fn apply_state(
        &self,
        network: &Network,
        revision: u64,
        desired: &settings::Network,
    ) -> Result<(), AnyError> {
        match desired.indexing {
            Requested::Paused => self.control.pause(network).await?,
            Requested::Running => {
                let paused = self
                    .health
                    .borrow()
                    .networks()
                    .iter()
                    .any(|row| row.state == indexing::NetworkState::Paused);
                let resume = self.applied.hold().resume;
                if paused || resume != desired.resume {
                    self.control.resume(network)?;
                }
            }
        }
        let mut applied = self.applied.hold();
        applied.resume = desired.resume;
        if applied.nodes == desired.nodes {
            applied.revision = revision;
        }
        self.access.open();
        Ok(())
    }
}

impl Runtime {
    pub(super) async fn reconcile(
        &self,
        network: &Network,
        revision: u64,
        desired: settings::Network,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), AnyError> {
        let current = self.active(network);
        if desired.nodes.is_empty() {
            if let Some(active) = current {
                active.access.block();
                active.control.pause(network).await?;
            }
            return Ok(());
        }
        if let Some(active) = &current
            && desired.indexing == Requested::Paused
        {
            active.control.pause(network).await?;
        }
        if current
            .as_ref()
            .is_none_or(|active| active.needs_connection(&desired))
        {
            self.connect(network, revision, &desired, current.as_ref(), shutdown)
                .await?;
        }
        let Some(active) = self.active(network) else {
            return Err("no verified RPC node".into());
        };
        if let Some(rebuild) = desired.rebuild.clone() {
            return self.rebuild(network, &active, &desired, rebuild).await;
        }
        active.apply_state(network, revision, &desired).await
    }

    async fn connect(
        &self,
        network: &Network,
        revision: u64,
        desired: &settings::Network,
        current: Option<&Arc<ActiveNetwork>>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), AnyError> {
        let mut nodes = desired.nodes.iter().collect::<Vec<_>>();
        if let Some(active) = current.filter(|a| a.statistics().snapshot().consecutive_failures > 0)
        {
            let id = active.node();
            nodes.sort_by_key(|node| node.id == id);
        }
        for node in nodes {
            let verified = tokio::select! {
                result = NodeConnection::verify(self, network, desired, node) => result,
                _ = shutdown.wait_for(|stop| *stop) => return Ok(()),
            };
            let Ok(candidate) = verified else {
                continue;
            };
            if !self.settings.current(network, revision) {
                return Ok(());
            }
            match current {
                Some(active) => {
                    active
                        .replace(self, network, revision, desired, candidate)
                        .await?
                }
                None => self.activate(network, revision, desired, candidate).await?,
            }
            self.publish()?;
            self.status
                .logger
                .with("chain", network.chain.to_string())
                .with("network", network.name.clone())
                .event(
                    logging::Level::Info,
                    "rpc.activated",
                    "Verified RPC node activated",
                    &[("node_id", node.id.as_str())],
                );
            return Ok(());
        }
        Err("no node passed chain verification".into())
    }

    async fn activate(
        &self,
        network: &Network,
        revision: u64,
        desired: &settings::Network,
        candidate: NodeConnection,
    ) -> Result<(), AnyError> {
        let applied = ConnectionState::new(revision, desired, &candidate);
        let submissions = Arc::new(wallets::Submissions::new());
        let registrar: Arc<dyn wallets::SubmissionRegistrar> = submissions.clone();
        let composed = wallets::Composition::build::<_, wallets::WalletError>(
            candidate.verified,
            self.repository.clone(),
            &registrar,
        )
        .await?;
        self.restore(&composed, network, revision).await?;
        let (report, readiness) = indexing::SyncReport::watching(composed.index.networks());
        let supervisor =
            indexing::Supervisor::new(composed.index.clone(), composed.schedules, report)
                .with_logger(self.status.logger.clone());
        let control = supervisor.control();
        if desired.indexing == Requested::Paused || desired.rebuild.is_some() {
            control.request_pause(network)?;
        }
        let (shutdown, receiver) = watch::channel(false);
        let active = Arc::new(ActiveNetwork {
            indexer: composed.index.units()[0].indexer(),
            wallets: composed.wallets,
            units: composed.units,
            history: composed.history,
            balance: composed.balance,
            access: Arc::new(Access::new()),
            health: readiness.receiver(),
            control,
            connection: candidate.connection,
            submissions,
            shutdown,
            task: Mutex::new(Some(tokio::spawn(supervisor.run(receiver)))),
            applied: Mutex::new(applied),
        });
        if desired.rebuild.is_some() {
            active.access.block();
        }
        self.active.hold().insert(network.clone(), active);
        Ok(())
    }

    async fn restore(
        &self,
        composed: &wallets::Composition<AssetTag>,
        network: &Network,
        revision: u64,
    ) -> Result<(), AnyError> {
        let signers = crate::api::Signers::new();
        self.registry
            .restore(&composed.wallets, &composed.index, &signers)
            .await?;
        for import in self
            .imports
            .iter()
            .filter(|i| composed.wallets.asset(i.asset()).is_ok())
        {
            import
                .clone()
                .import(
                    &composed.wallets,
                    &*composed.index,
                    &signers,
                    &self.registry,
                )
                .await?;
        }
        if !self.settings.current(network, revision) {
            return Err("configuration changed during wallet restoration".into());
        }
        self.signers.extend(&signers)?;
        Ok(())
    }
}
