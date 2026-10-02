//! What the supervisor exists for: one chain's failure is only its own.
//!
//! Every case drives two units through scripted pass results and asserts on
//! the events the monitor saw, the health it published, and whether
//! `Supervisor::run` is still supervising.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use indexing::{
    BackfillStatus, Birthday, BlockHash, BlockHeight, BlockPosition, BlockRef, BoxFuture,
    ChainName, Checkpoint, Holder, Index, IndexError, IndexErrorKind, Indexing, Network,
    NetworkState, SyncHealth, SyncPhase, SyncStatus, Watch, Watching,
};
use indexing::{NetworkEvent, Schedules, Supervisor, SyncControl, SyncMonitor};
use tokio::sync::watch;

/// What one scripted pass does.
#[derive(Clone)]
enum Outcome {
    Ready,
    Status(Box<SyncStatus>),
    CatchingUp,
    /// The pass succeeded and its backfill did not: forward indexing reached
    /// the tip, and the historical range the network still owes was refused.
    BackfillFails,
    Fails(IndexError),
    Panics,
    /// Never completes, so a test can stop the script at a known point.
    Parks,
    Wait(Arc<tokio::sync::Semaphore>),
}

/// One chain's indexer, scripted pass by pass.
struct Probe {
    network: Network,
    script: Mutex<VecDeque<Outcome>>,
    then: Outcome,
    calls: AtomicUsize,
}

impl Probe {
    fn new(network: &Network, script: impl IntoIterator<Item = Outcome>, then: Outcome) -> Self {
        Self {
            network: network.clone(),
            script: Mutex::new(script.into_iter().collect()),
            then,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Checkpoint for Probe {
    fn checkpoint<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async { Ok(None) })
    }
}

impl Watching for Probe {
    fn watch<'a>(
        &'a self,
        _id: &'a str,
        _subject: &'a dyn Holder,
        _start: Birthday,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        Box::pin(async { unreachable!("supervision never watches") })
    }

    fn unwatch<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
        Box::pin(async { unreachable!("supervision never unwatches") })
    }

    fn watched<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl indexing::Backfilling for Probe {
    fn cancel_backfill<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        Box::pin(async { unreachable!("supervision never cancels a backfill") })
    }
}

impl Indexing for Probe {
    fn network(&self) -> &Network {
        &self.network
    }

    fn listen(&self, _listener: Arc<dyn indexing::Listener>) {}

    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let outcome = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| self.then.clone());
        let network = self.network.clone();
        Box::pin(async move {
            let (phase, backfill) = match outcome {
                Outcome::Status(status) => return Ok(*status),
                Outcome::Ready => (SyncPhase::Ready, None),
                Outcome::Wait(gate) => {
                    gate.acquire().await.unwrap().forget();
                    (SyncPhase::Ready, None)
                }
                Outcome::CatchingUp => (SyncPhase::CatchingUp, None),
                Outcome::BackfillFails => (SyncPhase::Ready, Some(refused())),
                Outcome::Fails(error) => return Err(error),
                Outcome::Panics => panic!("scripted panic"),
                Outcome::Parks => std::future::pending().await,
            };
            Ok(SyncStatus {
                network,
                checkpoint: Some(block()),
                pending_tip: None,
                observed_tip: Some(block()),
                phase,
                unmatched_spends: Vec::new(),
                backfill,
            })
        })
    }
}

/// Records every event and publishes the snapshot a test waits on.
struct Recorder {
    events: Mutex<Vec<(String, NetworkState, NetworkState)>>,
    report: Arc<indexing::SyncReport>,
}

impl SyncMonitor for Recorder {
    fn network_observed(&self, health: &indexing::NetworkHealth) {
        self.report.network_observed(health);
    }

    fn network_changed(&self, event: &NetworkEvent) {
        self.events.lock().expect("events").push((
            event.health.status.network.chain.to_string(),
            event.previous,
            event.health.state,
        ));
        self.report.network_changed(event);
    }
}

fn network(chain: &str) -> Network {
    Network {
        chain: ChainName::new(chain),
        name: "testnet".into(),
    }
}

fn block() -> BlockRef {
    BlockRef {
        position: BlockPosition(4),
        height: BlockHeight(4),
        hash: BlockHash(vec![4; 32]),
        parent: None,
        timestamp: None,
    }
}

/// What one pass reports after a historical read its node refused.
fn refused() -> BackfillStatus {
    BackfillStatus {
        pending: 1,
        resume: BlockPosition(0),
        anchor: BlockPosition(4),
        inspected: 0,
        failure: Some(IndexError::new(
            IndexErrorKind::Source,
            "block is not available on this node",
            true,
        )),
    }
}

fn terminal() -> IndexError {
    IndexError::new(
        IndexErrorKind::InvalidBlock,
        "block contains a duplicate transaction",
        false,
    )
}

/// The whole fixture: two units, the supervisor task driving them, and the
/// handles a test observes and stops them with.
struct Supervised {
    task: tokio::task::JoinHandle<()>,
    stop: watch::Sender<bool>,
    health: watch::Receiver<SyncHealth>,
    control: SyncControl,
    recorder: Arc<Recorder>,
    probes: Vec<Arc<Probe>>,
}

impl Supervised {
    fn new(probes: Vec<Arc<Probe>>) -> Self {
        Self::every(probes, Duration::from_millis(1))
    }

    fn every(probes: Vec<Arc<Probe>>, interval: Duration) -> Self {
        let indexers = probes
            .iter()
            .map(|probe| Arc::clone(probe) as Arc<dyn Indexing>)
            .collect();
        let composer = Arc::new(Index::of(indexers).expect("disjoint networks compose"));
        let (report, readiness) = indexing::SyncReport::watching(composer.networks());
        let health = readiness.receiver();
        let recorder = Arc::new(Recorder {
            events: Mutex::new(Vec::new()),
            report,
        });
        let supervisor = Supervisor::new(
            composer,
            Schedules::every(interval),
            Arc::clone(&recorder) as Arc<dyn SyncMonitor>,
        );
        let control = supervisor.control();
        let (stop, shutdown) = watch::channel(false);
        Self {
            task: tokio::spawn(supervisor.run(shutdown)),
            stop,
            health,
            control,
            recorder,
            probes,
        }
    }

    /// Waits for `chain`'s network to reach `state`, or fails the test.
    async fn reaches(&mut self, chain: &str, state: NetworkState) {
        let wanted = network(chain);
        let reached = self.health.wait_for(|health| {
            health
                .networks()
                .iter()
                .any(|row| row.status.network == wanted && row.state == state)
        });
        tokio::time::timeout(Duration::from_secs(5), reached)
            .await
            .unwrap_or_else(|_| panic!("{chain} never reached {state:?}"))
            .expect("the supervisor keeps publishing");
    }

    /// Waits for `chain`'s network to report a stuck backfill, or fails the
    /// test.
    async fn reports_failure(&mut self, chain: &str) -> IndexError {
        let wanted = network(chain);
        let row = |health: &SyncHealth| {
            health
                .networks()
                .iter()
                .find(|row| row.status.network == wanted)
                .and_then(|row| row.status.backfill.as_ref()?.failure.clone())
        };
        let reported = self.health.wait_for(|health| row(health).is_some());
        let health = tokio::time::timeout(Duration::from_secs(5), reported)
            .await
            .unwrap_or_else(|_| panic!("{chain} never reported a failure"))
            .expect("the supervisor keeps publishing");
        row(&health).expect("the awaited failure")
    }

    fn events(&self) -> Vec<(String, NetworkState, NetworkState)> {
        self.recorder.events.lock().expect("events").clone()
    }

    async fn shut_down(self) {
        self.stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("shutdown must end every unit task")
            .expect("the supervisor never panics");
    }

    /// Drops the shutdown sender without ever sending `true`, which
    /// `Supervisor::run` documents as a shutdown of its own, and waits for
    /// supervision to end.
    async fn abandoned(self) {
        drop(self.stop);
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("a closed shutdown channel must end every unit task")
            .expect("the supervisor never panics");
    }
}

#[tokio::test]
async fn one_network_failure_does_not_stop_the_others() {
    let failing = Arc::new(Probe::new(
        &network("failing"),
        [],
        Outcome::Fails(terminal()),
    ));
    let healthy = Arc::new(Probe::new(&network("healthy"), [], Outcome::Ready));
    let mut supervised = Supervised::new(vec![Arc::clone(&failing), Arc::clone(&healthy)]);

    supervised
        .reaches("failing", NetworkState::Quarantined)
        .await;
    supervised.reaches("healthy", NetworkState::Ready).await;

    let before = healthy.calls();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        healthy.calls() > before,
        "the healthy chain keeps syncing while the other is quarantined"
    );
    assert!(
        !supervised.task.is_finished(),
        "a chain failure must not end the supervisor"
    );
    supervised.shut_down().await;
    assert_eq!(
        failing.calls(),
        1,
        "a terminal failure is never retried automatically"
    );
}

#[tokio::test]
async fn a_non_retryable_failure_quarantines_only_its_network() {
    let failing = Arc::new(Probe::new(
        &network("failing"),
        [],
        Outcome::Fails(terminal()),
    ));
    let healthy = Arc::new(Probe::new(&network("healthy"), [], Outcome::Ready));
    let mut supervised = Supervised::new(vec![failing, healthy]);

    supervised
        .reaches("failing", NetworkState::Quarantined)
        .await;
    supervised.reaches("healthy", NetworkState::Ready).await;

    let quarantined = supervised.health.borrow().clone();
    let reported = quarantined
        .networks()
        .iter()
        .find(|row| row.status.network == network("failing"))
        .expect("the failing network is reported")
        .clone();
    let failure = reported.failure.expect("a quarantined network reports why");
    assert_eq!(failure.kind, IndexErrorKind::InvalidBlock);
    assert!(!failure.retryable);
    assert_eq!(reported.attempts, 1);
    assert!(
        quarantined.ever_settled() && quarantined.ready(),
        "one quarantined chain must not hold readiness at false forever"
    );
    supervised.shut_down().await;
}

#[tokio::test]
async fn a_quarantined_unit_still_observes_shutdown() {
    let failing = Arc::new(Probe::new(
        &network("failing"),
        [],
        Outcome::Fails(terminal()),
    ));
    let parked = Arc::new(Probe::new(&network("parked"), [], Outcome::Parks));
    let mut supervised = Supervised::new(vec![failing, parked]);

    supervised
        .reaches("failing", NetworkState::Quarantined)
        .await;

    // A unit parked on a quarantine and a unit parked inside an RPC call both
    // have to notice shutdown, or the process hangs instead of exiting.
    supervised.shut_down().await;
}

#[tokio::test]
async fn a_panicking_unit_is_quarantined_not_propagated() {
    let panicking = Arc::new(Probe::new(&network("panicking"), [], Outcome::Panics));
    let healthy = Arc::new(Probe::new(&network("healthy"), [], Outcome::Ready));
    let mut supervised = Supervised::new(vec![panicking, healthy]);

    supervised
        .reaches("panicking", NetworkState::Quarantined)
        .await;
    supervised.reaches("healthy", NetworkState::Ready).await;
    assert!(
        !supervised.task.is_finished(),
        "a panicked unit task must not take the supervisor with it"
    );

    let calls = supervised.probes[0].calls();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        supervised.probes[0].calls(),
        calls,
        "a respawned unit starts parked, so a repeating panic cannot busy-loop"
    );
    supervised.shut_down().await;
}

#[tokio::test]
async fn resume_clears_quarantine_and_runs_a_pass_at_once() {
    let recovering = Arc::new(Probe::new(
        &network("recovering"),
        [Outcome::Fails(terminal())],
        Outcome::Ready,
    ));
    let healthy = Arc::new(Probe::new(&network("healthy"), [], Outcome::Ready));
    let mut supervised = Supervised::new(vec![recovering, healthy]);

    supervised
        .reaches("recovering", NetworkState::Quarantined)
        .await;
    assert_eq!(
        supervised
            .control
            .resume(&network("unconfigured"))
            .expect_err("an unconfigured network cannot be resumed")
            .kind,
        IndexErrorKind::NetworkMismatch
    );

    supervised
        .control
        .resume(&network("recovering"))
        .expect("a configured network resumes");
    supervised.reaches("recovering", NetworkState::Ready).await;

    let events: Vec<_> = supervised
        .events()
        .into_iter()
        .filter(|(chain, ..)| chain == "recovering")
        .map(|(_, previous, state)| (previous, state))
        .collect();
    assert_eq!(
        events,
        [
            (NetworkState::CatchingUp, NetworkState::Quarantined),
            (NetworkState::Quarantined, NetworkState::CatchingUp),
            (NetworkState::CatchingUp, NetworkState::Ready),
        ],
        "a resume reports the way back, not just the arrival"
    );
    supervised.shut_down().await;
}

/// A failure changes what the network is doing and never what its last
/// completed pass reported. The health holds that status whole, so the
/// checkpoint an operator reads stays where indexing got to, and the phase
/// beside it is that pass's answer rather than the network's state.
#[tokio::test]
async fn a_failed_pass_keeps_the_status_of_the_last_one_that_completed() {
    let failing = Arc::new(Probe::new(
        &network("failing"),
        [Outcome::Ready],
        Outcome::Fails(terminal()),
    ));
    let mut supervised = Supervised::new(vec![failing]);

    supervised
        .reaches("failing", NetworkState::Quarantined)
        .await;

    let row = supervised.health.borrow().networks()[0].clone();
    assert_eq!(row.failure, Some(terminal()));
    assert_eq!(row.attempts, 1);
    assert_eq!(
        row.status,
        SyncStatus {
            network: network("failing"),
            checkpoint: Some(block()),
            pending_tip: None,
            observed_tip: Some(block()),
            phase: SyncPhase::Ready,
            unmatched_spends: Vec::new(),
            backfill: None,
        },
        "the quarantine froze the last completed pass, it did not rewrite it"
    );
    supervised.shut_down().await;
}

#[tokio::test]
async fn monitor_fires_on_every_attempt_but_not_on_every_catch_up_batch() {
    let retrying = IndexError::new(IndexErrorKind::Source, "node busy", true);
    let scripted = Arc::new(Probe::new(
        &network("scripted"),
        [
            Outcome::CatchingUp,
            Outcome::CatchingUp,
            Outcome::CatchingUp,
            Outcome::Ready,
            Outcome::Fails(retrying.clone()),
            Outcome::Fails(retrying),
        ],
        Outcome::Parks,
    ));
    let mut supervised = Supervised::new(vec![Arc::clone(&scripted)]);

    supervised.reaches("scripted", NetworkState::Retrying).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while scripted.calls() < 7 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the scripted unit runs every pass");

    let states: Vec<_> = supervised
        .events()
        .into_iter()
        .map(|(_, previous, state)| (previous, state))
        .collect();
    assert_eq!(
        states,
        [
            (NetworkState::CatchingUp, NetworkState::Ready),
            (NetworkState::Ready, NetworkState::Retrying),
            (NetworkState::Retrying, NetworkState::Retrying),
        ],
        "three catch-up batches are silent; every failed attempt is not"
    );
    assert_eq!(
        supervised.health.borrow().networks()[0].attempts,
        2,
        "a wedged loop must stay distinguishable from a first failure"
    );
    supervised.shut_down().await;
}

/// A backfill that keeps failing is scheduled and reported, not laundered into
/// productive catch-up.
///
/// A pass carrying a backfill failure used to be indistinguishable from one
/// that had merely not finished catching up: the attempt counter reset, the
/// unit asked for the next pass at once, and the failure was dropped from the
/// health row on its way past. A node with a pruned range therefore spun at
/// full speed with nothing to show for it and nothing saying so.
#[tokio::test]
async fn a_failing_backfill_is_reported_and_waited_on_rather_than_retried_at_once() {
    let stalled = Arc::new(Probe::new(&network("stalled"), [], Outcome::BackfillFails));
    let mut supervised = Supervised::every(vec![Arc::clone(&stalled)], Duration::from_millis(100));

    let failure = supervised.reports_failure("stalled").await;
    assert_eq!(failure.kind, IndexErrorKind::Source);
    assert!(failure.retryable);

    let calls = stalled.calls();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        stalled.calls() - calls < 5,
        "a failing backfill waits between attempts instead of spinning: {} passes in 250ms",
        stalled.calls() - calls
    );

    let row = supervised.health.borrow().networks()[0].clone();
    assert_eq!(
        row.state,
        NetworkState::Ready,
        "forward indexing is healthy, so the network is not quarantined or retrying"
    );
    assert!(row.failure.is_none(), "nothing holds forward indexing back");
    assert!(
        row.attempts > 1,
        "and repeated failures stay distinguishable from the first"
    );
    supervised.shut_down().await;
}

/// The availability floor : a watch over a range
/// the node will never serve must not take the API down with it.
///
/// This is what made the omission a live defect rather than a missing
/// feature. Configured wallets are imported *before* the supervisor starts
/// (`apps/server/src/main.rs`), and the listener binds only once every
/// network has settled, so one such wallet stopped the process serving
/// anything at all — on any chain — for as long as the node stayed pruned,
/// which is forever. What the range weakens is the reads that touch it, and
/// those already say so through `Completeness`.
#[tokio::test]
async fn a_backfill_that_can_never_be_served_does_not_hold_readiness_down() {
    let stalled = Arc::new(Probe::new(&network("stalled"), [], Outcome::BackfillFails));
    let healthy = Arc::new(Probe::new(&network("healthy"), [], Outcome::Ready));
    let mut supervised = Supervised::new(vec![stalled, healthy]);

    supervised.reaches("stalled", NetworkState::Ready).await;
    supervised.reaches("healthy", NetworkState::Ready).await;

    let health = supervised.health.borrow().clone();
    assert!(
        health.ever_settled(),
        "startup binds its listener instead of waiting for a range that cannot close"
    );
    assert!(health.ready(), "and the readiness probe answers 204");
    let row = health
        .networks()
        .iter()
        .find(|row| row.status.network == network("stalled"))
        .expect("the stalled network")
        .clone();
    let backfill = row
        .status
        .backfill
        .expect("the stuck range stays visible to an operator");
    assert_eq!(backfill.resume, BlockPosition(0));
    assert_eq!(backfill.anchor, BlockPosition(4));
    assert_eq!(
        backfill.failure.expect("naming why it is stuck").message,
        "block is not available on this node"
    );
    supervised.shut_down().await;
}

/// The other half of the same rule: an index that has not reached its tip is
/// not ready, and never becomes ready by having nothing owed.
#[tokio::test]
async fn a_network_that_has_not_reached_its_tip_is_still_not_ready() {
    let behind = Arc::new(Probe::new(&network("behind"), [], Outcome::CatchingUp));
    let mut supervised = Supervised::new(vec![behind]);

    supervised.reaches("behind", NetworkState::CatchingUp).await;

    let health = supervised.health.borrow().clone();
    assert!(!health.ready(), "an index behind its tip answers 503");
    assert!(!health.ever_settled(), "and the listener waits for it");
    supervised.shut_down().await;
}

/// The same availability floor as the two cases above, one layer further in:
/// an internal handshake is not an outage either.
///
/// A retryable [`IndexErrorKind::Conflict`] is by construction a lost race
/// between two in-process actors — a `Watching::watch` publishing an address's
/// filter, and the supervised pass that captured its plan a moment earlier.
/// No block, checkpoint, node or store changed, and the next pass reconciles,
/// which is why `Unit::applied` already gives a conflict the healthy cadence
/// rather than a backoff. Publishing `Retrying` for it anyway made the network
/// *state* disagree with that: every `POST /v1/addresses` that happened to
/// land inside a pass flapped `ready` to false for one poll interval, so
/// admitting an address depooled the process from its own load balancer for
/// doing exactly what the client asked.
///
/// What this gives up is deliberate and small: a single lost race is now
/// silent — no event, no `failure`, no attempt count on the published row. A
/// conflict that stops reconciling needs no report of its own, because the
/// checkpoint stops tracking the tip and the next pass that completes says so
/// as `CatchingUp`, which does hold readiness down.
#[tokio::test]
async fn an_admission_conflict_is_not_an_outage_and_never_dips_readiness() {
    let conflict = IndexError::new(
        IndexErrorKind::Conflict,
        "address admission is changing",
        true,
    );
    let racing = Arc::new(Probe::new(
        &network("racing"),
        [Outcome::Ready, Outcome::Fails(conflict)],
        Outcome::Parks,
    ));
    let mut supervised = Supervised::new(vec![Arc::clone(&racing)]);

    supervised.reaches("racing", NetworkState::Ready).await;
    // The third pass parks, so the unit is frozen on whatever the conflict
    // left behind and the assertions below race nothing.
    tokio::time::timeout(Duration::from_secs(5), async {
        while racing.calls() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the racing unit runs the pass after its conflict");

    let health = supervised.health.borrow().clone();
    assert!(
        health.ready(),
        "admitting an address must not answer 503 on /health/ready"
    );
    let row = health.networks()[0].clone();
    assert_eq!(
        row.state,
        NetworkState::Ready,
        "the checkpoint is still at the tip, so the network is still ready"
    );
    assert!(
        row.failure.is_none(),
        "nothing is holding forward indexing back: {:?}",
        row.failure
    );
    assert_eq!(
        row.attempts, 0,
        "a lost race is not an attempt that got nowhere"
    );
    assert_eq!(
        supervised
            .events()
            .into_iter()
            .map(|(_, previous, state)| (previous, state))
            .collect::<Vec<_>>(),
        [(NetworkState::CatchingUp, NetworkState::Ready)],
        "and it is not a transition, so the monitor never sees one"
    );
    supervised.shut_down().await;
}

/// The other half of the same discrimination: a conflict the pass reported as
/// terminal is still a quarantine, so the early return above cannot swallow
/// one.
#[tokio::test]
async fn a_terminal_conflict_is_still_quarantined() {
    let wedged = Arc::new(Probe::new(
        &network("wedged"),
        [],
        Outcome::Fails(IndexError::new(
            IndexErrorKind::Conflict,
            "indexers contain the same network",
            false,
        )),
    ));
    let mut supervised = Supervised::new(vec![Arc::clone(&wedged)]);

    supervised
        .reaches("wedged", NetworkState::Quarantined)
        .await;
    let row = supervised.health.borrow().networks()[0].clone();
    assert_eq!(
        row.failure.expect("a quarantined network reports why").kind,
        IndexErrorKind::Conflict
    );
    assert!(
        !supervised.health.borrow().ready(),
        "the only network is quarantined, so nothing useful can be served"
    );
    supervised.shut_down().await;
}

/// Closing the shutdown channel ends supervision, whatever its last value was.
///
/// `Supervisor::run` documents the dropped sender as a shutdown, so the value
/// the channel holds at that moment is `false` and no unit may read it as
/// "carry on". A unit whose pass is still outstanding when the channel closes
/// is the case that matters: its wait ends at once and there is nothing left to
/// wait for, so a loop that only checks the last *value* runs again
/// immediately, for as long as the process lives.
///
/// Two workers, because the spin this guards against never yields: a
/// single-threaded runtime would hang here instead of failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_shutdown_channel_ends_every_unit() {
    let parked = Arc::new(Probe::new(&network("parked"), [], Outcome::Parks));
    let supervised = Supervised::new(vec![Arc::clone(&parked)]);
    started(&parked).await;

    supervised.abandoned().await;

    assert_eq!(
        parked.calls(),
        1,
        "a closed channel must end the unit, not make it start pass after pass"
    );
}

/// Waits until `probe`'s first pass is outstanding, so the channel closes while
/// the unit is inside its own `select!` rather than before it started.
async fn started(probe: &Arc<Probe>) {
    let begun = async {
        while probe.calls() == 0 {
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), begun)
        .await
        .expect("the unit starts its first pass");
}

#[tokio::test]
async fn ready_snapshots_advance_without_extra_transition_notifications() {
    let network = network("advancing");
    let ready = |position: u64| {
        let block = |at| BlockRef {
            position: BlockPosition(at),
            height: BlockHeight(at),
            hash: BlockHash(at.to_be_bytes().to_vec()),
            parent: None,
            timestamp: None,
        };
        Outcome::Status(Box::new(SyncStatus {
            network: network.clone(),
            checkpoint: Some(block(position)),
            pending_tip: Some(block(position + 1)),
            observed_tip: Some(block(position + 1)),
            phase: SyncPhase::Ready,
            unmatched_spends: Vec::new(),
            backfill: None,
        }))
    };
    let probe = Arc::new(Probe::new(&network, [ready(4), ready(8)], Outcome::Parks));
    let supervised = Supervised::new(vec![probe.clone()]);
    tokio::time::timeout(Duration::from_secs(5), async {
        while probe.calls() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both ready passes completed");
    let current = supervised.health.borrow().networks()[0].clone();
    assert_eq!(
        current
            .status
            .checkpoint
            .as_ref()
            .map(|block| block.position),
        Some(BlockPosition(8))
    );
    assert_eq!(
        current
            .status
            .pending_tip
            .as_ref()
            .map(|block| block.position),
        Some(BlockPosition(9))
    );
    assert_eq!(
        supervised.events().len(),
        1,
        "only the initial Ready transition is an event"
    );
    supervised.shut_down().await;
}

#[tokio::test]
async fn pause_acknowledges_idle_and_resume_restarts_only_that_network() {
    let first = Arc::new(Probe::new(&network("first"), [], Outcome::Ready));
    let second = Arc::new(Probe::new(&network("second"), [], Outcome::Ready));
    let mut running = Supervised::new(vec![first.clone(), second.clone()]);
    running.reaches("first", NetworkState::Ready).await;
    running.control.pause(&network("first")).await.unwrap();
    running.reaches("first", NetworkState::Paused).await;
    let calls = first.calls();
    let other = second.calls();
    tokio::time::timeout(Duration::from_secs(2), async {
        while second.calls() <= other {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(first.calls(), calls);
    running.control.resume(&network("first")).unwrap();
    running.reaches("first", NetworkState::Ready).await;
    assert!(first.calls() > calls);
    running.stop.send(true).unwrap();
    running.task.await.unwrap();
    assert!(running.control.pause(&network("first")).await.is_err());
}

#[tokio::test]
async fn pause_waits_for_active_pass_without_cancelling_it() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let probe = Arc::new(Probe::new(
        &network("first"),
        [Outcome::Wait(gate.clone())],
        Outcome::Ready,
    ));
    let running = Supervised::new(vec![probe.clone()]);
    tokio::time::timeout(Duration::from_secs(2), async {
        while probe.calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let network = network("first");
    let pause = running.control.pause(&network);
    tokio::pin!(pause);
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(pause.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), pause)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate.available_permits(), 0);
    assert_eq!(probe.calls(), 1);
    running.stop.send(true).unwrap();
    running.task.await.unwrap();
}

#[tokio::test]
async fn recovery_discards_reported_progress_before_resume() {
    let network = network("recovered");
    let probe = Arc::new(Probe::new(&network, [], Outcome::Ready));
    let mut running = Supervised::new(vec![probe.clone()]);
    running.reaches("recovered", NetworkState::Ready).await;
    assert!(running.control.discard_progress(&network).await.is_err());
    running.control.pause(&network).await.unwrap();
    assert!(
        running.health.borrow().networks()[0]
            .status
            .checkpoint
            .is_some()
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        running.control.discard_progress(&network),
    )
    .await
    .unwrap()
    .unwrap();
    let calls = probe.calls();
    {
        let health = running.health.borrow();
        let row = &health.networks()[0];
        assert_eq!(row.state, NetworkState::Paused);
        assert!(row.status.checkpoint.is_none());
        assert!(row.status.pending_tip.is_none());
        assert!(row.status.backfill.is_none());
    }
    assert_eq!(probe.calls(), calls);
    running.control.resume(&network).unwrap();
    running.reaches("recovered", NetworkState::Ready).await;
    running.shut_down().await;
}
