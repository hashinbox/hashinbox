//! An operator's requests, as revision-checked edits of one network's row.

use super::{Network, Phase, ReindexRequest, Requested, Settings};
use crate::config::AnyError;
use std::sync::Arc;

/// An operator's request for one network's indexing.
#[derive(Clone, Copy)]
pub(crate) enum Action {
    Pause,
    Resume,
    Reindex,
}

impl Settings {
    /// Persists an operator's `action` on one network, against the revision it
    /// was read at.
    ///
    /// # Errors
    ///
    /// An unconfigured network, a second reindex while one is pending, an
    /// exhausted resume counter, and a revision conflict.
    pub(crate) async fn command(
        self: &Arc<Self>,
        network: &base::Network,
        action: Action,
    ) -> Result<u64, AnyError> {
        self.amend(network, |saved| {
            match action {
                Action::Pause => saved.indexing = Requested::Paused,
                Action::Resume => {
                    saved.indexing = Requested::Running;
                    saved.resume = saved
                        .resume
                        .checked_add(1)
                        .ok_or("resume counter exhausted")?;
                }
                Action::Reindex => {
                    if saved.rebuild.is_some() {
                        return Err("reindex already requested".into());
                    }
                    saved.indexing = Requested::Running;
                    saved.rebuild = Some(ReindexRequest {
                        id: uuid::Uuid::now_v7().to_string(),
                        phase: Phase::Requested,
                        target: None,
                    });
                }
            }
            Ok(())
        })
        .await
    }

    /// Moves reindex request `id` of `network` to `next`; `None` completes it.
    ///
    /// # Errors
    ///
    /// When the saved request is no longer `id`, and a revision conflict.
    pub(crate) async fn rebuild(
        self: &Arc<Self>,
        network: &base::Network,
        id: &str,
        next: Option<ReindexRequest>,
    ) -> Result<u64, AnyError> {
        self.amend(network, |saved| {
            if saved.rebuild.as_ref().map(|r| r.id.as_str()) != Some(id) {
                return Err("reindex request changed".into());
            }
            saved.rebuild = next;
            Ok(())
        })
        .await
    }

    /// Read-modify-write of one network's row, checked against the revision
    /// it was read at.
    async fn amend(
        self: &Arc<Self>,
        network: &base::Network,
        change: impl FnOnce(&mut Network) -> Result<(), AnyError>,
    ) -> Result<u64, AnyError> {
        let chain = network.chain.to_string();
        let mut row = self.all().remove(&chain).ok_or("chain not configured")?;
        let saved = row
            .chain
            .networks
            .iter_mut()
            .find(|n| n.name == network.name)
            .ok_or("network not configured")?;
        change(saved)?;
        self.save(chain, Some(row.revision), row.chain).await
    }
}
