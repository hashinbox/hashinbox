//! The stored RPC nodes of every network, on the `rpc_endpoints` model.

use base::Network;

use super::Storage;
use crate::models::{NetworkEndpoints, RpcEndpoints, endpoint};

impl Storage {
    /// Every network's stored nodes.
    ///
    /// # Errors
    ///
    /// The read, or a row that no longer checks.
    pub(crate) async fn endpoints(&self) -> anyhow::Result<NetworkEndpoints> {
        self.read(endpoint::db::find_all).await
    }

    /// Makes `endpoints` the stored nodes of `network`.
    ///
    /// # Errors
    ///
    /// The write.
    pub(crate) async fn set_endpoints(
        &self,
        network: &Network,
        endpoints: RpcEndpoints,
    ) -> anyhow::Result<()> {
        let network = network.clone();
        self.write(move |connection| endpoint::db::upsert(connection, &network, &endpoints))
            .await
    }
}
