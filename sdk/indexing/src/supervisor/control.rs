use std::{collections::BTreeMap, sync::Arc};

use crate::{Index, IndexError, IndexErrorKind, Indexing, Network};
use tokio::sync::watch;

/// Clears a quarantined network from outside the supervisor.
///
/// A [`SyncMonitor`](super::SyncMonitor) reports what indexing decided; this
/// is what an operator decides back. It is a handle rather than a callback
/// return value because the decision — "the cause was fixed out of band, try
/// again" — arrives long after the event that reported the failure.
///
/// Cheap to clone; every clone reaches the same supervisor.
#[derive(Clone)]
pub struct SyncControl {
    /// Which unit owns each configured network.
    networks: Arc<BTreeMap<Network, usize>>,
    /// One resume counter per unit; a parked unit waits for its next change.
    units: Arc<Vec<watch::Sender<u64>>>,
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
            units: Arc::new(units),
        }
    }

    /// Clears a quarantine: the attempt count resets and the next pass runs at
    /// once. A network that is not quarantined is a no-op.
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
        self.units[*unit].send_modify(|resumes| *resumes = resumes.wrapping_add(1));
        Ok(())
    }

    /// The resume signal of one unit. A fresh receiver has seen the current
    /// value, so a resume issued before the quarantine does not clear it.
    pub(crate) fn resumes(&self, unit: usize) -> watch::Receiver<u64> {
        self.units[unit].subscribe()
    }
}
