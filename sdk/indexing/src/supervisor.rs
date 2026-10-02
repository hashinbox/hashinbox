//! One task per composed unit, so one chain's failure does not stop the others
//! (cargo feature `supervisor`).

mod control;
mod monitor;
mod readiness;
mod unit;

use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::watch,
    task::{Id, JoinSet},
};

use crate::{Index, IndexError, IndexErrorKind, Indexing, Schedule, Schedules};

pub use control::SyncControl;
pub use monitor::{NetworkEvent, SyncMonitor};
pub use readiness::{Readiness, SyncReport};
use unit::Unit;

/// Drives every unit of one composed index, each on its own task.
///
/// One task per [`SyncUnit`](crate::SyncUnit) is what makes chain failures
/// independent: a unit that fails terminally quarantines its own networks and
/// parks, a unit whose task dies is respawned parked, and every other unit
/// keeps syncing at its own cadence.
///
/// `C` is what the composed children answer, defaulting to [`Indexing`]. Driving
/// a pass needs no read at all, so a composition of stores that answer none is
/// supervised exactly the same way, as `Supervisor<dyn Indexing>`.
pub struct Supervisor<C: ?Sized = dyn Indexing> {
    pub(crate) units: Arc<Index<C>>,
    pub(crate) schedules: Schedules,
    pub(crate) monitor: Arc<dyn SyncMonitor>,
    pub(crate) control: SyncControl,
    pub(crate) logger: logging::Logger,
}

impl<C: Indexing + ?Sized + 'static> Supervisor<C> {
    /// Supervises every unit of `units`, polling each on its own schedule and
    /// reporting every network transition to `monitor`.
    ///
    /// There is no selection argument: each unit reads its own watch list.
    #[must_use]
    pub fn new(units: Arc<Index<C>>, schedules: Schedules, monitor: Arc<dyn SyncMonitor>) -> Self {
        let control = SyncControl::new(&units);
        Self {
            units,
            schedules,
            monitor,
            control,
            logger: logging::Logger::default(),
        }
    }

    /// Records meaningful indexing transitions using a caller-owned sink.
    #[must_use]
    pub fn with_logger(mut self, logger: logging::Logger) -> Self {
        self.logger = logger;
        self
    }

    /// The handle that clears quarantines. Take it before [`Self::run`]
    /// consumes the supervisor.
    #[must_use]
    pub fn control(&self) -> SyncControl {
        self.control.clone()
    }

    /// Supervises the unit tasks until shutdown.
    ///
    /// Returns only after `shutdown` becomes `true`, or its sender is dropped,
    /// and every unit task has ended. No chain failure returns early: that is
    /// the whole point of the supervision, and a quarantined unit stays parked
    /// and alive so the shutdown sequence still finds one task to await.
    pub async fn run(self, shutdown: watch::Receiver<bool>) {
        let mut tasks = JoinSet::new();
        let mut owners = HashMap::new();
        for index in 0..self.units.units().len() {
            self.spawn(&mut tasks, &mut owners, index, &shutdown, None);
        }
        while let Some(ended) = tasks.join_next_with_id().await {
            let Err(error) = ended else {
                continue;
            };
            let Some(index) = owners.remove(&error.id()) else {
                continue;
            };
            if *shutdown.borrow() {
                continue;
            }
            // A panicked or aborted unit task is a quarantine, never a reason
            // to stop supervising the others. It is respawned parked, so a
            // resume still reaches it and a repeating panic cannot busy-loop.
            let failure = IndexError::new(
                IndexErrorKind::Store,
                format!("synchronization task ended unexpectedly: {error}"),
                false,
            );
            self.spawn(&mut tasks, &mut owners, index, &shutdown, Some(&failure));
        }
        self.control.stop();
    }

    fn spawn(
        &self,
        tasks: &mut JoinSet<()>,
        owners: &mut HashMap<Id, usize>,
        index: usize,
        shutdown: &watch::Receiver<bool>,
        failure: Option<&IndexError>,
    ) {
        let mut unit = Unit::new(self, index);
        if let Some(failure) = failure {
            unit.quarantine(failure);
        }
        let task = tasks.spawn(unit.drive(shutdown.clone()));
        owners.insert(task.id(), index);
    }
}
