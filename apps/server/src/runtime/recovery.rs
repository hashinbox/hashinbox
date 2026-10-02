//! Reindex phases are persisted before destructive work; retries preserve registrations.
use super::{Runtime, network::ActiveNetwork};
use crate::{
    config::AnyError,
    settings::{self, Phase, Requested},
};
use base::Network;
use indexing::{IndexStore as _, Purging as _, WatchStore as _};

impl Runtime {
    pub(super) async fn rebuild(
        &self,
        network: &Network,
        active: &ActiveNetwork,
        desired: &settings::Network,
        mut request: settings::ReindexRequest,
    ) -> Result<(), AnyError> {
        active.access.block();
        if desired.indexing == Requested::Paused {
            active.control.pause(network).await?;
            return Ok(());
        }
        if request.phase != Phase::Scanning {
            self.purge(network, active, &mut request).await?;
            return Ok(());
        }
        if active
            .health
            .borrow()
            .networks()
            .iter()
            .any(|row| row.state == indexing::NetworkState::Paused)
        {
            active.control.resume(network)?;
        }
        let ready =
            active.health.borrow().networks().iter().any(|row| {
                row.state == indexing::NetworkState::Ready && row.status.backfill.is_none()
            });
        if request.target.is_none() && !ready {
            return Ok(());
        }
        if !self.rebuilt(network, request.target).await? {
            return Ok(());
        }
        self.settings.rebuild(network, &request.id, None).await?;
        active.access.open();
        self.status
            .logger
            .with("chain", network.chain.to_string())
            .with("network", network.name.clone())
            .event(
                logging::Level::Info,
                "indexing.rebuilt",
                "Network reindex completed",
                &[],
            );
        Ok(())
    }

    async fn purge(
        &self,
        network: &Network,
        active: &ActiveNetwork,
        request: &mut settings::ReindexRequest,
    ) -> Result<(), AnyError> {
        let _admission = active.access.idle().await?;
        if !active.submissions.idle()? {
            active.control.resume(network)?;
            return Err("waiting for background submissions before reindex".into());
        }
        active.control.pause(network).await?;
        if request.phase == Phase::Requested {
            request.target = self
                .repository
                .checkpoint(network)
                .await?
                .map(|block| block.position.0);
            request.phase = Phase::Purging;
            self.settings
                .rebuild(network, &request.id, Some(request.clone()))
                .await?;
        }
        self.repository.purge_network(network).await?;
        active.indexer.reset().await?;
        active.control.discard_progress(network).await?;
        self.status
            .logs
            .clear(&logging::Filter(std::collections::BTreeMap::from([
                ("chain".into(), network.chain.to_string()),
                ("network".into(), network.name.clone()),
            ])));
        request.phase = Phase::Scanning;
        self.settings
            .rebuild(network, &request.id, Some(request.clone()))
            .await?;
        active.control.resume(network)?;
        Ok(())
    }

    async fn rebuilt(&self, network: &Network, target: Option<u64>) -> Result<bool, AnyError> {
        let checkpoint = self.repository.checkpoint(network).await?;
        let Some(target) = target.or_else(|| checkpoint.as_ref().map(|block| block.position.0))
        else {
            return Ok(false);
        };
        if checkpoint
            .as_ref()
            .is_none_or(|block| block.position.0 < target)
        {
            return Ok(false);
        }
        let watches = self.repository.all(network).await?;
        Ok(watches.iter().all(|watch| {
            watch.watch.start.0 > target
                || watch
                    .indexed_through
                    .as_ref()
                    .is_some_and(|block| block.position.0 >= target)
        }))
    }
}
