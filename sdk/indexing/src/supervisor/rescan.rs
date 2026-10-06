//! The online rescan of one network, as one operation of its controller.

use std::{error, fmt};

use super::SyncControl;
use crate::{IndexError, Indexing, PurgedRows, Purging};

/// Why an online rescan ([`SyncControl::rescan`]) stopped, and where that
/// left the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RescanError {
    /// Nothing was purged: the pause failed, or the purge itself did. The
    /// network was resumed as it was.
    NotStarted(IndexError),
    /// The purge committed and a later step failed. The network stays
    /// paused; rescanning it again finishes the window, because a second
    /// purge removes nothing.
    Interrupted(IndexError),
}

impl RescanError {
    /// The failure underneath, whichever step it stopped at.
    #[must_use]
    pub const fn source(&self) -> &IndexError {
        match self {
            Self::NotStarted(error) | Self::Interrupted(error) => error,
        }
    }
}

impl fmt::Display for RescanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted(error) => write!(formatter, "rescan did not start: {error}"),
            Self::Interrupted(error) => write!(
                formatter,
                "rescan stopped after the purge and the network is paused: {error}"
            ),
        }
    }
}

impl error::Error for RescanError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(self.source())
    }
}

impl SyncControl {
    /// Purges `indexer`'s network from `store` and indexes it again from its
    /// watches' birthdays, while every other network keeps syncing: the
    /// quiesced window [`Purging`] describes, in its order — [`Self::pause`],
    /// [`Purging::purge_network`], [`Indexing::reset`],
    /// [`Self::discard_progress`], [`Self::resume`]. Returns once the network
    /// is scanning again; the scan itself runs on in the supervisor.
    ///
    /// The window's other half is the caller's: block the network's
    /// admissions (watch registrations and sends) before calling this and
    /// reopen them after, and run one rescan of a network at a time.
    ///
    /// Nothing is persisted between the steps: before the purge nothing has
    /// changed, and after it a restarted process scans from the birthdays
    /// without purging again.
    ///
    /// # Errors
    ///
    /// [`RescanError::NotStarted`] when the pause or the purge failed, the
    /// network resumed unchanged; [`RescanError::Interrupted`] when a step
    /// after the purge failed, the network left paused.
    pub async fn rescan(
        &self,
        indexer: &dyn Indexing,
        store: &dyn Purging,
    ) -> Result<PurgedRows, RescanError> {
        let network = indexer.network();
        self.pause(network).await.map_err(RescanError::NotStarted)?;
        let purged = match store.purge_network(network).await {
            Ok(purged) => purged,
            Err(error) => {
                // Resuming a network this controller just paused cannot fail.
                let _ = self.resume(network);
                return Err(RescanError::NotStarted(error));
            }
        };
        indexer.reset().await.map_err(RescanError::Interrupted)?;
        self.discard_progress(network)
            .await
            .map_err(RescanError::Interrupted)?;
        self.resume(network).map_err(RescanError::Interrupted)?;
        Ok(purged)
    }
}
