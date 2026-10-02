//! What the runtime observes about itself: logs, per-network failures and
//! per-node RPC statistics. Memory only; nothing here is persisted.
use crate::lock::Lock as _;
use base::Network;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(crate) struct Status {
    pub(crate) logs: Arc<logging::Memory>,
    pub(crate) logger: logging::Logger,
    errors: Mutex<BTreeMap<Network, String>>,
    nodes: Mutex<BTreeMap<(Network, String), json_rpc::Statistics>>,
}

impl Status {
    /// The only failure prose a network reports: fixed, so no remote error
    /// text or endpoint reaches an operator page.
    const UNAVAILABLE: &'static str = "Configuration or node unavailable; check RPC logs";

    pub(crate) fn new() -> Self {
        let logs = Arc::new(logging::Memory::default());
        Self {
            logger: logging::Logger::new(logs.clone()),
            logs,
            errors: Mutex::new(BTreeMap::new()),
            nodes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Records whether `network`'s last reconciliation failed.
    pub(crate) fn failed(&self, network: &Network, failed: bool) {
        let mut errors = self.errors.hold();
        if failed {
            errors.insert(network.clone(), Self::UNAVAILABLE.into());
        } else {
            errors.remove(network);
        }
    }

    pub(crate) fn error(&self, network: &Network) -> Option<String> {
        self.errors.hold().get(network).cloned()
    }

    /// Keeps the statistics of one node of `network` for the settings page.
    pub(crate) fn track(&self, network: &Network, node: &str, statistics: json_rpc::Statistics) {
        self.nodes
            .hold()
            .insert((network.clone(), node.to_owned()), statistics);
    }

    pub(crate) fn node(&self, network: &Network, node: &str) -> Option<json_rpc::Statistics> {
        self.nodes
            .hold()
            .get(&(network.clone(), node.to_owned()))
            .cloned()
    }
}
