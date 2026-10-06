use std::{collections::BTreeMap, sync::Arc};

use crate::{Index, IndexError, IndexErrorKind, Indexing, Network};
use tokio::sync::watch;

/// Pauses or resumes one network without stopping other supervised networks.
///
/// A [`SyncMonitor`](super::SyncMonitor) reports what indexing decided; this
/// is what an operator decides back. It is a handle rather than a callback
/// return value because the decision — "the cause was fixed out of band, try
/// again" — arrives long after the event that reported the failure.
///
/// **Online rescan of one network.** The controller of a network may purge it
/// while every other network keeps syncing: block the network's admissions
/// (the application's), then [`Self::rescan`], which runs [`Self::pause`],
/// [`crate::Purging::purge_network`], [`crate::Indexing::reset`],
/// [`Self::discard_progress`] and [`Self::resume`] in that order, then reopen
/// the admissions. The next pass re-derives the network from its registered
/// birthdays. Reads are not gated here: the purged store answers them honestly
/// by itself ([`crate::Purging`]). A restart after the purge resumes scanning
/// without purging again.
///
/// Cheap to clone; every clone reaches the same supervisor.
#[derive(Clone)]
pub struct SyncControl {
    /// Which unit owns each configured network.
    networks: Arc<BTreeMap<Network, usize>>,
    /// One resume counter per unit; a parked unit waits for its next change.
    units: Arc<Vec<watch::Sender<u64>>>,
    paused: Arc<Vec<watch::Sender<bool>>>,
    acknowledgements: Arc<Vec<watch::Sender<bool>>>,
    discarded: Arc<Vec<watch::Sender<bool>>>,
    stopped: watch::Sender<bool>,
}

impl SyncControl {
    pub(crate) fn new<C: Indexing + ?Sized>(composer: &Index<C>) -> Self {
        let mut owners = BTreeMap::new();
        let mut units = Vec::with_capacity(composer.units().len());
        for (index, unit) in composer.units().iter().enumerate() {
            owners.insert(unit.network().clone(), index);
            units.push(watch::channel(0).0);
        }
        Self {
            networks: Arc::new(owners),
            paused: Arc::new((0..units.len()).map(|_| watch::channel(false).0).collect()),
            acknowledgements: Arc::new((0..units.len()).map(|_| watch::channel(false).0).collect()),
            discarded: Arc::new((0..units.len()).map(|_| watch::channel(false).0).collect()),
            units: Arc::new(units),
            stopped: watch::channel(false).0,
        }
    }

    /// Clears a pause or quarantine; the next pass runs at once.
    /// A running network is woken early without interrupting its active pass.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::NetworkMismatch`] when the network is not configured.
    pub fn resume(&self, network: &Network) -> Result<(), IndexError> {
        let unit = self.networks.get(network).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "index network is not configured",
                false,
            )
        })?;
        self.acknowledgements[*unit].send_replace(false);
        self.paused[*unit].send_replace(false);
        self.units[*unit].send_modify(|resumes| *resumes = resumes.wrapping_add(1));
        Ok(())
    }

    /// Requests a pause before starting a supervisor, or without waiting for acknowledgment.
    ///
    /// # Errors
    /// Refuses an unknown network.
    pub fn request_pause(&self, network: &Network) -> Result<(), IndexError> {
        let unit = *self.networks.get(network).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "index network is not configured",
                false,
            )
        })?;
        self.paused[unit].send_replace(true);
        self.units[unit].send_modify(|resumes| *resumes = resumes.wrapping_add(1));
        Ok(())
    }

    /// Requests a pause and waits for the active bounded pass to finish.
    /// Does not cancel a commit. Prevent other drivers and concurrent control commands
    /// at the caller; a pause controls only this supervisor.
    ///
    /// # Errors
    /// Unknown network, or shutdown before acknowledgment.
    pub async fn pause(&self, network: &Network) -> Result<(), IndexError> {
        let unit = *self.networks.get(network).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "index network is not configured",
                false,
            )
        })?;
        if *self.stopped.borrow() {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "index supervisor stopped",
                true,
            ));
        }
        let mut acknowledged = self.acknowledgements[unit].subscribe();
        self.paused[unit].send_replace(true);
        self.units[unit].send_modify(|resumes| *resumes = resumes.wrapping_add(1));
        let mut stopped = self.stopped.subscribe();
        tokio::select! {
            result = acknowledged.wait_for(|paused| *paused) =>
                result.map(|_| ()).map_err(|_| IndexError::new(
                IndexErrorKind::Conflict, "index supervisor stopped before pause", true)),
            _ = stopped.wait_for(|stopped| *stopped) => Err(IndexError::new(
                IndexErrorKind::Conflict, "index supervisor stopped before pause", true)),
        }
    }

    pub(crate) fn stop(&self) {
        self.stopped.send_replace(true);
    }

    /// Clears reported progress after a purge and indexer reset.
    /// The network must remain paused; serialize recovery and control commands.
    /// Returns only after the monitor has observed the empty progress.
    ///
    /// # Errors
    /// Unknown network, missing pause acknowledgment, or a stopped supervisor.
    pub async fn discard_progress(&self, network: &Network) -> Result<(), IndexError> {
        let unit = *self.networks.get(network).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "unknown index network",
                false,
            )
        })?;
        if !self.paused(unit) || !*self.acknowledgements[unit].borrow() {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "pause indexing before discarding progress",
                false,
            ));
        }
        let mut discarded = self.discarded[unit].subscribe();
        self.discarded[unit].send_replace(true);
        self.units[unit].send_modify(|resumes| *resumes = resumes.wrapping_add(1));
        let mut stopped = self.stopped.subscribe();
        tokio::select! {
            result = discarded.wait_for(|requested| !*requested) =>
                result.map(|_| ()).map_err(|_| IndexError::new(
                    IndexErrorKind::Conflict, "supervisor stopped during recovery", true)),
            _ = stopped.wait_for(|stopped| *stopped) => Err(IndexError::new(
                IndexErrorKind::Conflict, "supervisor stopped during recovery", true)),
        }
    }

    pub(crate) fn discarding(&self, unit: usize) -> bool {
        *self.discarded[unit].borrow()
    }

    pub(crate) fn discarded(&self, unit: usize) {
        self.discarded[unit].send_replace(false);
    }

    pub(crate) fn paused(&self, unit: usize) -> bool {
        *self.paused[unit].borrow()
    }

    pub(crate) fn acknowledge(&self, unit: usize, paused: bool) {
        self.acknowledgements[unit].send_replace(paused);
    }

    /// The resume signal of one unit. A fresh receiver has seen the current
    /// value, so a resume issued before the quarantine does not clear it.
    pub(crate) fn resumes(&self, unit: usize) -> watch::Receiver<u64> {
        self.units[unit].subscribe()
    }
}
