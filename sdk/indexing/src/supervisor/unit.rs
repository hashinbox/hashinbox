use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    Index, IndexError, IndexErrorKind, Indexing, NetworkHealth, NetworkState, SyncPhase, SyncStatus,
};
use tokio::sync::watch;

use super::{NetworkEvent, Schedule, Supervisor, SyncControl, SyncMonitor};

/// Why a unit's pass or wait ended.
enum Wake {
    /// A pass completed with this result.
    ///
    /// Boxed because one status is the whole of what a pass reports now, and it
    /// is the only variant carrying anything: inline, it would size every
    /// `Wake` a wait produces after the fact that nothing was reported.
    Pass(Box<Result<SyncStatus, IndexError>>),
    /// Something other than shutdown woke the unit; start again.
    Again,
    /// Shutdown was requested, or its channel closed.
    Stop,
}

/// One supervised unit: its own loop over one child indexer, its state
/// machine, and the health of the one network that child owns.
///
/// Nothing here can end the supervisor. A terminal failure parks this unit
/// alive until a resume or shutdown, and the other units keep syncing.
pub(crate) struct Unit<C: ?Sized> {
    units: Arc<Index<C>>,
    index: usize,
    schedule: Schedule,
    monitor: Arc<dyn SyncMonitor>,
    control: SyncControl,
    health: NetworkHealth,
    attempts: u32,
}

impl<C: Indexing + ?Sized + 'static> Unit<C> {
    /// The unit at `index` of `supervisor`'s composition, not started yet.
    pub(crate) fn new(supervisor: &Supervisor<C>, index: usize) -> Self {
        let network = supervisor.units.units()[index].network();
        Self {
            units: Arc::clone(&supervisor.units),
            index,
            schedule: supervisor.schedules.of(network),
            monitor: Arc::clone(&supervisor.monitor),
            control: supervisor.control.clone(),
            health: NetworkHealth::starting(network),
            attempts: 0,
        }
    }

    /// Quarantines this unit's network before it starts, reporting `failure`.
    /// The supervisor uses it to respawn a unit whose task died.
    pub(crate) fn quarantine(&mut self, failure: &IndexError) {
        self.attempts = self.attempts.saturating_add(1);
        self.report(NetworkState::Quarantined, Some(failure));
    }

    /// Syncs this unit until shutdown, waiting, retrying, or parking between
    /// passes as its state machine decides.
    ///
    /// **Every wait answers [`Wake`], and every `Wake::Stop` returns.** The
    /// value the shutdown channel holds is not the whole of shutdown: closing
    /// it is the other half, documented on
    /// [`Supervisor::run`](super::Supervisor::run), and it leaves the last
    /// value at whatever it was — `false`, for a process that drops the sender
    /// rather than setting it. A loop that read only `*shutdown.borrow()` would
    /// see "carry on", find its wait already over, and run again at once for as
    /// long as the process lived, starting a fresh pass on each turn.
    pub(crate) async fn drive(mut self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.quarantined() {
                match self.park(&mut shutdown).await {
                    Wake::Stop => return,
                    _ => continue,
                }
            }
            let result = match self.pass(&mut shutdown).await {
                Wake::Pass(result) => *result,
                Wake::Stop => return,
                Wake::Again => continue,
            };
            let Some(wait) = self.applied(result) else {
                continue;
            };
            if matches!(self.waited(wait, &mut shutdown).await, Wake::Stop) {
                return;
            }
        }
    }

    fn quarantined(&self) -> bool {
        self.health.state == NetworkState::Quarantined
    }

    async fn pass(&self, shutdown: &mut watch::Receiver<bool>) -> Wake {
        let unit = &self.units.units()[self.index];
        tokio::select! {
            result = unit.sync() => Wake::Pass(Box::new(result)),
            changed = shutdown.changed() => Self::woke(changed.is_err(), shutdown),
        }
    }

    async fn waited(&self, wait: Duration, shutdown: &mut watch::Receiver<bool>) -> Wake {
        tokio::select! {
            changed = shutdown.changed() => Self::woke(changed.is_err(), shutdown),
            () = tokio::time::sleep(wait) => Wake::Again,
        }
    }

    /// Parks a quarantined unit: it stays alive, holding its last checkpoint,
    /// until an operator resumes it or the process shuts down. It must not
    /// return, or the supervisor would see its task end before shutdown.
    async fn park(&mut self, shutdown: &mut watch::Receiver<bool>) -> Wake {
        let mut resumes = self.control.resumes(self.index);
        let woken = tokio::select! {
            changed = resumes.changed() => {
                if changed.is_err() { Wake::Stop } else { Wake::Again }
            }
            changed = shutdown.changed() => Self::woke(changed.is_err(), shutdown),
        };
        if matches!(woken, Wake::Again) {
            self.attempts = 0;
            self.report(NetworkState::CatchingUp, None);
        }
        woken
    }

    fn woke(closed: bool, shutdown: &watch::Receiver<bool>) -> Wake {
        if closed || *shutdown.borrow() {
            Wake::Stop
        } else {
            Wake::Again
        }
    }

    /// Applies one pass result and answers how long to wait before the next
    /// pass. `None` means "run again at once"; a unit this quarantined parks
    /// at the top of the loop instead.
    fn applied(&mut self, result: Result<SyncStatus, IndexError>) -> Option<Duration> {
        let error = match result {
            Ok(status) => return self.progressed(status),
            Err(error) => error,
        };
        self.attempts = self.attempts.saturating_add(1);
        let retrying = error.retryable;
        // A retryable `Conflict` is a lost race against another in-process
        // actor, not an outage: the watch admission handshake refused one pass
        // so an address could be published, and the next pass reconciles. So
        // it keeps the network's state as well as the healthy cadence. Moving
        // it to `Retrying` published a readiness dip for every `POST
        // /v1/addresses` that happened to land inside a pass, which depools
        // the process from its own load balancer for doing what a client
        // asked. A conflict that
        // stops reconciling needs no report of its own: the checkpoint stops
        // tracking the tip, and the next pass that completes says so as
        // `CatchingUp`.
        if retrying && error.kind == IndexErrorKind::Conflict {
            return Some(self.schedule.interval);
        }
        let state = if retrying {
            NetworkState::Retrying
        } else {
            NetworkState::Quarantined
        };
        self.report(state, Some(&error));
        retrying.then_some(Self::jittered(self.schedule.retry(self.attempts)))
    }

    fn progressed(&mut self, status: SyncStatus) -> Option<Duration> {
        let now = Self::now();
        let stalled = Self::stalled(&status).is_some();
        self.attempts = if stalled {
            self.attempts.saturating_add(1)
        } else {
            0
        };
        let state = match status.phase {
            SyncPhase::Ready => NetworkState::Ready,
            SyncPhase::CatchingUp => NetworkState::CatchingUp,
        };
        self.publish(
            NetworkHealth {
                state,
                // The pass succeeded, so forward indexing has no failure
                // to report; a backfill's does travel, on the status that
                // names the range it belongs to.
                failure: None,
                attempts: self.attempts,
                since: now,
                status,
            },
            now,
        );
        if stalled {
            return Some(Self::jittered(self.schedule.retry(self.attempts)));
        }
        (state == NetworkState::Ready).then_some(self.schedule.interval)
    }

    /// The backfill failure a *successful* pass carried, if any.
    ///
    /// It is the one way a pass can succeed and still have made no progress.
    /// The forward phase is fine — a network behind its tip spends the whole
    /// batch there and attempts no backfill at all, so a backfill failure
    /// means forward indexing already reached the tip — but the historical
    /// read was refused, and asking for it again at once re-issues the same
    /// refused call for as long as the range stays unreadable, which on a
    /// pruned node is forever. So it is counted, waited on with the same
    /// capped and jittered backoff a failed pass gets, and reported
    /// (`AGENTS.md`: bounded failure retries).
    ///
    /// It is deliberately not a state change, and deliberately not a hold on
    /// readiness either. Forward synchronization is what an operator's
    /// clients read and it is healthy, so the network reports whatever the
    /// tip says it is; the failure travels on the status
    /// [`NetworkHealth`] keeps, beside the range that is stuck, instead of
    /// being dropped or turned into an outage.
    fn stalled(status: &SyncStatus) -> Option<&IndexError> {
        status.backfill.as_ref()?.failure.as_ref()
    }

    /// Moves this unit's network to `state`, keeping its last good checkpoint:
    /// a failure never rewrites where indexing got to.
    fn report(&mut self, state: NetworkState, failure: Option<&IndexError>) {
        let now = Self::now();
        let attempts = self.attempts;
        self.publish(
            NetworkHealth {
                state,
                failure: failure.cloned(),
                attempts,
                ..self.health.clone()
            },
            now,
        );
    }

    /// Records `next` for its network and notifies the monitor when the network
    /// transitioned, or failed one more time.
    ///
    /// "Failed one more time" covers a repeating [`NetworkState::Retrying`]
    /// and a pass that succeeded while carrying a backfill failure: both are
    /// attempts that did not get anywhere, and a unit whose state never
    /// changes would otherwise report the first one and then go quiet.
    fn publish(&mut self, mut next: NetworkHealth, now: u64) {
        if next.status.network != self.health.status.network {
            return;
        }
        let previous = self.health.state;
        next.since = if previous == next.state {
            self.health.since
        } else {
            now
        };
        let stalled = Self::stalled(&next.status).is_some();
        self.health.clone_from(&next);
        self.monitor.network_observed(&next);
        if previous != next.state
            || next.state == NetworkState::Retrying
            || next.failure.is_some()
            || stalled
        {
            self.monitor.network_changed(&NetworkEvent {
                previous,
                health: next,
            });
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs())
    }

    /// Spreads the retries of networks that failed together over the last
    /// quarter of their wait. The clock's sub-second remainder is enough
    /// entropy for a wait that is already bounded, and it costs no dependency.
    fn jittered(wait: Duration) -> Duration {
        const RANGE: u64 = 1024;
        const FLOOR: u64 = 768;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| u64::from(since.subsec_nanos()));
        let scale = FLOOR + nanos % (RANGE - FLOOR);
        let wait = u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX);
        Duration::from_nanos(wait / RANGE * scale)
    }
}
