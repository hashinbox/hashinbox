//! [`Networks`]: every stored network with its nodes and routes, read in
//! one go; registered into the container once at start, and loaded into a
//! network's transport after every edit.

use base::{Container, InfoClient, NetworkConfig, ServiceFactory};

use super::Transport;
use crate::{
    db::Db,
    models::{Network, network},
};

/// Every stored network: its config, nodes and routes, as the database holds
/// them.
pub(crate) struct Networks(Vec<Network>);

impl Networks {
    /// The networks with their assets, nodes, routes and index: one read.
    ///
    /// # Errors
    ///
    /// The read.
    pub(crate) async fn read(db: &Db) -> anyhow::Result<Self> {
        Ok(Self(db.read(network::db::find_all).await?))
    }

    /// Every indexed network of `F::CHAIN`: a transport read by `I`
    /// (`Transport::new::<I>`), loaded with its nodes and routes, registered
    /// with `F`. Main only, once per chain.
    pub(crate) fn register<I: InfoClient + 'static, F: ServiceFactory>(
        &self,
        container: &Container<Transport>,
    ) -> &Self {
        let indexed = self.0.iter().filter(|network| network.index.is_some());
        for network in indexed.filter(|network| network.key().chain == F::CHAIN) {
            let config = NetworkConfig::from(network);
            let transport = Transport::new::<I>(&config);
            // Every stored URL was checked when stored; one that no longer
            // builds is left out, and the API shows it without state.
            let _ = transport.load(&network.endpoints, &network.routes);
            container.register::<F>(config, transport);
        }
        self
    }

    /// `network`'s stored nodes and routes into its transport
    /// (`Transport::load`). No chain types: the transport kept its reader.
    ///
    /// # Errors
    ///
    /// A network that is not registered, or what the transport refuses.
    pub(crate) fn load(
        &self,
        network: &base::Network,
        container: &Container<Transport>,
    ) -> anyhow::Result<()> {
        let transport = container.transport(network)?;
        let Some(stored) = self.0.iter().find(|stored| stored.key() == *network) else {
            return transport.load(&[], &[]);
        };
        transport.load(&stored.endpoints, &stored.routes)
    }
}
