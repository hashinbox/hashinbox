//! **Contract.**
//!
//! One task per composed unit, so one chain's failure does not stop the others
//! (cargo feature `supervisor`).

use crate::{
    Condition, Health, IndexError, IndexErrorKind, Indexers, Indexing, Network, Outcome, Phase,
    PurgedRows, Schedule, Schedules, SyncHealth,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{error, fmt};
use tokio::sync::watch;
use tokio::task::{Id, JoinSet};

/// Drives every unit of one composed index, each on its own task.
///
/// One task per [`Route`](crate::Route) is what makes chain failures
/// independent: a unit that fails terminally quarantines its own networks and
/// parks, a unit whose task dies is respawned parked, and every other unit
/// keeps syncing at its own cadence.
///
/// `C` is what the composed children answer, defaulting to [`Indexing`]. Driving
/// a pass needs no read at all, so a composition of stores that answer none is
/// supervised exactly the same way, as `Supervisor<dyn Indexing>`.
pub struct Supervisor<C: ?Sized = dyn Indexing> {
    pub(crate) units: Arc<Indexers<C>>,
    pub(crate) schedules: Schedules,
    pub(crate) monitor: Arc<dyn Monitor>,
    pub(crate) control: Control,
    pub(crate) logger: logging::Logger,
}

impl<C: Indexing + ?Sized + 'static> Supervisor<C> {
    /// Supervises every unit of `units`, polling each on its own schedule and
    /// reporting every network transition to `monitor`.
    ///
    /// There is no selection argument: each unit reads its own watch list.
    #[must_use]
    pub fn new(units: Arc<Indexers<C>>, schedules: Schedules, monitor: Arc<dyn Monitor>) -> Self {
        let control = Control::new(&units);
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
    pub fn control(&self) -> Control {
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
        let mut unit = Worker::new(self, index);
        if let Some(failure) = failure {
            unit.quarantine(failure);
        }
        let task = tasks.spawn(unit.drive(shutdown.clone()));
        owners.insert(task.id(), index);
    }
}

/// Pauses or resumes one network without stopping other supervised networks.
///
/// A [`Monitor`](super::Monitor) reports what indexing decided; this
/// is what an operator decides back. It is a handle rather than a callback
/// return value because the decision — "the cause was fixed out of band, try
/// again" — arrives long after the event that reported the failure.
///
/// **Online rescan of one network.** The controller of a network may purge it
/// while every other network keeps syncing: block the network's admissions
/// (the application's), then [`Self::rescan`], which runs [`Self::pause`],
/// [`IndexStore::purge`], [`crate::Indexing::reset`],
/// [`Self::discard_progress`] and [`Self::resume`] in that order, then reopen
/// the admissions. The next pass re-derives the network from its registered
/// birthdays. Reads are not gated here: the purged store answers them honestly
/// by itself ([`crate::IndexStore::purge`]). A restart after the purge resumes scanning
/// without purging again.
///
/// Cheap to clone; every clone reaches the same supervisor.
#[derive(Clone)]
pub struct Control {
    /// Which unit owns each configured network.
    networks: Arc<BTreeMap<Network, usize>>,
    /// One resume counter per unit; a parked unit waits for its next change.
    units: Arc<Vec<watch::Sender<u64>>>,
    paused: Arc<Vec<watch::Sender<bool>>>,
    acknowledgements: Arc<Vec<watch::Sender<bool>>>,
    discarded: Arc<Vec<watch::Sender<bool>>>,
    stopped: watch::Sender<bool>,
}

impl Control {
    pub(crate) fn new<C: Indexing + ?Sized>(composer: &Indexers<C>) -> Self {
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

/// Observes network snapshots.
///
/// Called inline on the reporting unit's task, between passes, so it must
/// return promptly: hand work to a queue or a channel rather than doing I/O
/// here. It is synchronous by construction, and so is
/// [`Listener::committed`](crate::Listener::committed): a hook that cannot
/// await cannot slow a pass by awaiting, which is the mistake an awaited
/// callback used to make.
///
/// It is notification-only. Whether a failure is retried or quarantined is
/// already determined by the error's kind and retryability, and the one
/// decision an operator makes out of band is
/// [`Control::resume`](super::Control::resume).
pub trait Monitor: Send + Sync + 'static {
    /// Samples the latest network status after every completed pass or state update.
    /// Implementations must return promptly and perform no I/O.
    fn network_observed(&self, health: &Health);
}

/// The monitor that keeps one [`SyncHealth`] snapshot current, and publishes
/// it to the [`Readiness`] a caller waits on.
///
/// Every consumer of a [`Supervisor`](super::Supervisor) needs somewhere for a
/// transition to land and a way to wait until the index has caught up. The
/// rule itself stays where it was: [`SyncHealth`] owns it.
///
/// It does no I/O, as [`Monitor`] requires, and it writes nothing: an
/// application that wants a line per transition implements [`Monitor`]
/// itself and forwards `network_observed` here.
pub struct Report {
    snapshot: watch::Sender<SyncHealth>,
    /// What every network looks like before it has reported, kept so
    /// [`Self::reset`] can say exactly that again.
    starting: SyncHealth,
}

/// Waits until every network a [`Report`] watches has settled.
pub struct Readiness(watch::Receiver<SyncHealth>);

impl Report {
    /// A monitor over `networks`, none of them reported yet, and the readiness
    /// it publishes to.
    ///
    /// The networks are the composition's own
    /// ([`Indexers::networks`](crate::Indexers::networks)); a transition on any
    /// other is ignored.
    #[must_use]
    pub fn watching(networks: &[Network]) -> (Arc<Self>, Readiness) {
        let starting = SyncHealth::starting(networks);
        let (snapshot, readiness) = watch::channel(starting.clone());
        (Arc::new(Self { snapshot, starting }), Readiness(readiness))
    }

    /// Reports every network as unreported again, so nothing reads as ready.
    ///
    /// A shutdown sequence calls this first: the supervisor is still running
    /// and will publish nothing to say so, and a probe answering "ready" while
    /// the process is stopping admits work that cannot be finished.
    pub fn reset(&self) {
        self.snapshot
            .send_modify(|snapshot| snapshot.clone_from(&self.starting));
    }
}

impl Monitor for Report {
    fn network_observed(&self, health: &Health) {
        self.snapshot.send_modify(|snapshot| snapshot.apply(health));
    }
}

impl Readiness {
    /// Waits until every network has settled ([`SyncHealth::ever_settled`]):
    /// it reached its node's tip once, or it was quarantined. Returns at once
    /// when that already holds.
    ///
    /// A quarantined network does not hold this back, because it will not
    /// recover on its own; the snapshot returned says which networks are
    /// `Ready` and why the others are not.
    ///
    /// `None` once the [`Report`] is gone: the supervisor that owned it
    /// has stopped, so waiting any longer would wait for good.
    pub async fn settled(&mut self) -> Option<SyncHealth> {
        let settled = self.0.wait_for(SyncHealth::ever_settled).await.ok()?;
        Some(SyncHealth::clone(&settled))
    }

    /// The same snapshot for a consumer that polls rather than waits, such as
    /// a status route.
    #[must_use]
    pub fn receiver(&self) -> watch::Receiver<SyncHealth> {
        self.0.clone()
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use crate::{ChainName, Condition, Health};

    fn network(chain: &str) -> Network {
        Network {
            chain: ChainName::new(chain),
            name: "regtest".into(),
        }
    }

    fn event(network: &Network, state: Condition) -> Health {
        Health {
            state,
            ..Health::starting(network)
        }
    }

    #[tokio::test]
    async fn the_waiter_wakes_when_the_last_network_settles() {
        let (first, second) = (network("first"), network("second"));
        let (report, mut readiness) = Report::watching(&[first.clone(), second.clone()]);
        let waiting = tokio::spawn(async move { readiness.settled().await });

        report.network_observed(&event(&first, Condition::Ready));
        report.network_observed(&event(&second, Condition::Retrying));
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "one network has reached its tip and the other is still trying"
        );

        report.network_observed(&event(&second, Condition::Ready));

        let settled = waiting.await.expect("waiter").expect("a live report");
        assert!(settled.ready());
        assert!(
            settled
                .networks()
                .iter()
                .all(|row| row.state == Condition::Ready)
        );
    }

    #[tokio::test]
    async fn a_quarantined_network_does_not_hold_the_waiter_back() {
        let (first, second) = (network("first"), network("second"));
        let (report, mut readiness) = Report::watching(&[first.clone(), second.clone()]);
        let waiting = tokio::spawn(async move { readiness.settled().await });

        report.network_observed(&event(&first, Condition::Quarantined));
        report.network_observed(&event(&second, Condition::Ready));
        tokio::task::yield_now().await;

        assert!(waiting.is_finished(), "a quarantine heals by no waiting");
        let settled = waiting.await.expect("waiter").expect("a live report");
        assert_eq!(settled.networks()[0].state, Condition::Quarantined);
        assert!(settled.ready(), "the healthy network still serves");
    }

    #[tokio::test]
    async fn every_network_quarantined_still_settles() {
        let only = network("only");
        let (report, mut readiness) = Report::watching(std::slice::from_ref(&only));
        let waiting = tokio::spawn(async move { readiness.settled().await });

        report.network_observed(&event(&only, Condition::Quarantined));
        tokio::task::yield_now().await;

        assert!(waiting.is_finished(), "nothing is left to wait for");
        let settled = waiting.await.expect("waiter").expect("a live report");
        assert!(
            !settled.ready(),
            "and the snapshot says nothing can be served"
        );
    }

    #[tokio::test]
    async fn a_transition_on_an_unwatched_network_settles_nothing() {
        let (report, readiness) = Report::watching(&[network("watched")]);

        report.network_observed(&event(&network("other"), Condition::Ready));

        assert!(!readiness.receiver().borrow().ever_settled());
    }

    #[tokio::test]
    async fn a_reset_report_has_to_settle_again() {
        let only = network("only");
        let (report, mut readiness) = Report::watching(std::slice::from_ref(&only));
        report.network_observed(&event(&only, Condition::Ready));
        assert!(readiness.settled().await.is_some());

        report.reset();

        let snapshot = readiness.receiver().borrow().clone();
        assert!(!snapshot.ready());
        assert!(!snapshot.ever_settled());
    }

    #[tokio::test]
    async fn waiting_on_a_dropped_report_ends_rather_than_hangs() {
        let (report, mut readiness) = Report::watching(&[network("only")]);

        drop(report);

        assert!(readiness.settled().await.is_none());
    }
}

// The online rescan of one network, as one operation of its controller.

/// Why an online rescan ([`Control::rescan`]) stopped, and where that
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

impl Control {
    /// Purges `indexer`'s network from `store` and indexes it again from its
    /// watches' birthdays, while every other network keeps syncing: the
    /// quiesced window [`IndexStore::purge`] describes, in its order — [`Self::pause`],
    /// [`IndexStore::purge`], [`Indexing::reset`],
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
        store: &dyn IndexStore,
    ) -> Result<PurgedRows, RescanError> {
        let network = indexer.network();
        self.pause(network).await.map_err(RescanError::NotStarted)?;
        let purged = match store.purge(network).await {
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

/// Why a unit's pass or wait ended.
enum Wake {
    /// A pass completed with this result.
    ///
    /// Boxed because one status is the whole of what a pass reports now, and it
    /// is the only variant carrying anything: inline, it would size every
    /// `Wake` a wait produces after the fact that nothing was reported.
    Pass(Box<Result<Outcome, IndexError>>),
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
pub(crate) struct Worker<C: ?Sized> {
    units: Arc<Indexers<C>>,
    index: usize,
    schedule: Schedule,
    monitor: Arc<dyn Monitor>,
    control: Control,
    health: Health,
    attempts: u32,
    logger: logging::Logger,
    commands: watch::Receiver<u64>,
}

impl<C: Indexing + ?Sized + 'static> Worker<C> {
    /// The unit at `index` of `supervisor`'s composition, not started yet.
    pub(crate) fn new(supervisor: &Supervisor<C>, index: usize) -> Self {
        let network = supervisor.units.units()[index].network();
        Self {
            units: Arc::clone(&supervisor.units),
            index,
            schedule: supervisor.schedules.of(network),
            monitor: Arc::clone(&supervisor.monitor),
            control: supervisor.control.clone(),
            health: Health::starting(network),
            attempts: 0,
            commands: supervisor.control.resumes(index),
            logger: supervisor
                .logger
                .with("chain", network.chain.to_string())
                .with("network", network.name.clone())
                .with("component", "indexing"),
        }
    }

    /// Quarantines this unit's network before it starts, reporting `failure`.
    /// The supervisor uses it to respawn a unit whose task died.
    pub(crate) fn quarantine(&mut self, failure: &IndexError) {
        self.attempts = self.attempts.saturating_add(1);
        self.report(Condition::Quarantined, Some(failure));
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
            if self.control.paused(self.index) {
                if self.control.discarding(self.index) {
                    self.health = Health::starting(&self.health.status.network);
                    self.attempts = 0;
                }
                self.report(Condition::Paused, None);
                self.control.discarded(self.index);
                self.control.acknowledge(self.index, true);
                let wake = self.park(&mut shutdown).await;
                if matches!(wake, Wake::Stop) {
                    return;
                }
                continue;
            }
            self.control.acknowledge(self.index, false);
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
        self.health.state == Condition::Quarantined
    }

    async fn pass(&self, shutdown: &mut watch::Receiver<bool>) -> Wake {
        let unit = &self.units.units()[self.index];
        tokio::select! {
            result = unit.sync() => Wake::Pass(Box::new(result)),
            changed = shutdown.changed() => Self::woke(changed.is_err(), shutdown),
        }
    }

    async fn waited(&mut self, wait: Duration, shutdown: &mut watch::Receiver<bool>) -> Wake {
        if self.control.paused(self.index) {
            return Wake::Again;
        }
        tokio::select! {
            changed = shutdown.changed() => Self::woke(changed.is_err(), shutdown),
            () = tokio::time::sleep(wait) => Wake::Again,
            _ = self.commands.changed() => Wake::Again,
        }
    }

    /// Parks a quarantined unit: it stays alive, holding its last checkpoint,
    /// until an operator resumes it or the process shuts down. It must not
    /// return, or the supervisor would see its task end before shutdown.
    async fn park(&mut self, shutdown: &mut watch::Receiver<bool>) -> Wake {
        if self.health.state == Condition::Paused && !self.control.paused(self.index) {
            self.report(Condition::CatchingUp, None);
            return Wake::Again;
        }
        if self.quarantined() && self.control.paused(self.index) {
            return Wake::Again;
        }
        let woken = tokio::select! {
            changed = self.commands.changed() => {
                if changed.is_err() { Wake::Stop } else { Wake::Again }
            }
            changed = shutdown.changed() => Self::woke(changed.is_err(), shutdown),
        };
        if matches!(woken, Wake::Again) && !self.control.paused(self.index) {
            self.attempts = 0;
            self.report(Condition::CatchingUp, None);
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
    fn applied(&mut self, result: Result<Outcome, IndexError>) -> Option<Duration> {
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
            Condition::Retrying
        } else {
            Condition::Quarantined
        };
        if state == Condition::Quarantined {
            self.commands.borrow_and_update();
        }
        self.report(state, Some(&error));
        retrying.then_some(Self::jittered(self.schedule.retry(self.attempts)))
    }

    fn progressed(&mut self, status: Outcome) -> Option<Duration> {
        let now = Self::now();
        let stalled = Self::stalled(&status).is_some();
        self.attempts = if stalled {
            self.attempts.saturating_add(1)
        } else {
            0
        };
        let state = match status.phase {
            Phase::Ready => Condition::Ready,
            Phase::CatchingUp => Condition::CatchingUp,
        };
        self.publish(
            Health {
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
        (state == Condition::Ready).then_some(self.schedule.interval)
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
    /// [`Health`] keeps, beside the range that is stuck, instead of
    /// being dropped or turned into an outage.
    fn stalled(status: &Outcome) -> Option<&IndexError> {
        status.backfill.as_ref()?.failure.as_ref()
    }

    /// Moves this unit's network to `state`, keeping its last good checkpoint:
    /// a failure never rewrites where indexing got to.
    fn report(&mut self, state: Condition, failure: Option<&IndexError>) {
        let now = Self::now();
        let attempts = self.attempts;
        self.publish(
            Health {
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
    /// "Failed one more time" covers a repeating [`Condition::Retrying`]
    /// and a pass that succeeded while carrying a backfill failure: both are
    /// attempts that did not get anywhere, and a unit whose state never
    /// changes would otherwise report the first one and then go quiet.
    fn publish(&mut self, mut next: Health, now: u64) {
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
        if previous != next.state || next.attempts == 1 || next.attempts.is_power_of_two() {
            let level = if next.state == Condition::Quarantined {
                logging::Level::Error
            } else if next.failure.is_some() || stalled {
                logging::Level::Warn
            } else {
                logging::Level::Info
            };
            let failure = next
                .failure
                .as_ref()
                .or_else(|| Self::stalled(&next.status));
            let kind = failure
                .map(|error| format!("{:?}", error.kind))
                .unwrap_or_default();
            self.logger.event(
                level,
                "indexing.state",
                "Indexing state changed",
                &[
                    ("state", &format!("{:?}", next.state)),
                    ("failure", &kind),
                    ("attempts", &next.attempts.to_string()),
                ],
            );
        }
        self.health.clone_from(&next);
        self.monitor.network_observed(&next);
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
