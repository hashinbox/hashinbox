use std::sync::Arc;

use tokio::sync::watch;

use super::{NetworkEvent, SyncMonitor};
use crate::{Network, NetworkHealth, SyncHealth};

/// The monitor that keeps one [`SyncHealth`] snapshot current, and the
/// [`Readiness`] a caller waits on.
///
/// Every consumer of a [`Supervisor`](super::Supervisor) needs the same two
/// things: somewhere for a transition to land, and a way to wait until the
/// composition can answer questions. Writing that twice is how two callers
/// come to disagree about what "ready" means, so the snapshot and the waiting
/// live here and the rule itself stays where it always was —
/// [`SyncHealth::ready`] and [`SyncHealth::settled`] own it.
///
/// It does no I/O, as [`SyncMonitor`] requires, and it logs nothing: a library
/// crate must not write to standard output. An application that wants a line
/// per transition implements [`SyncMonitor`] itself and calls [`Self::publish`]
/// from it, which is what `apps/server` does.
pub struct SyncReport {
    snapshot: watch::Sender<SyncHealth>,
    /// What every network looks like before it has reported, kept so
    /// [`Self::reset`] can say exactly that again.
    starting: SyncHealth,
}

/// Waits until the networks a [`SyncReport`] watches reach a standing
/// condition, and hands the same snapshot to a consumer that polls it.
///
/// Both waiters resolve to `None` once the report is dropped: the supervisor
/// that owned it has stopped, so nothing further will ever be published and a
/// caller waiting for readiness would otherwise wait for good.
pub struct Readiness(watch::Receiver<SyncHealth>);

impl SyncReport {
    /// A monitor over `networks`, none of them reported yet, and the readiness
    /// it publishes to.
    ///
    /// The networks are the composition's own
    /// ([`Index::networks`](crate::Index::networks)); a transition on any
    /// other is ignored, because the snapshot describes the composition it was
    /// built from and nothing else.
    #[must_use]
    pub fn watching(networks: &[Network]) -> (Arc<Self>, Readiness) {
        let starting = SyncHealth::starting(networks);
        let (snapshot, readiness) = watch::channel(starting.clone());
        (
            Arc::new(Self {
                snapshot,
                starting,
            }),
            Readiness(readiness),
        )
    }

    /// Records one network's health in the snapshot.
    ///
    /// This is the whole of [`SyncMonitor::network_changed`]'s body, exposed
    /// so an application that wants its own diagnostics beside it can wrap
    /// this type instead of keeping a second channel.
    pub fn publish(&self, health: &NetworkHealth) {
        self.snapshot.send_modify(|snapshot| snapshot.apply(health));
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

impl SyncMonitor for SyncReport {
    fn network_changed(&self, event: &NetworkEvent) {
        self.publish(&event.health);
    }
}

impl Readiness {
    /// Waits until the composition can serve complete history for the chains
    /// it still answers for ([`SyncHealth::ready`]).
    pub async fn ready(&mut self) -> Option<SyncHealth> {
        self.reached(SyncHealth::ready).await
    }

    /// Waits until every network has settled ([`SyncHealth::settled`]): the
    /// gate a startup sequence binds its listener behind.
    pub async fn settled(&mut self) -> Option<SyncHealth> {
        self.reached(SyncHealth::settled).await
    }

    /// What every network is right now, without waiting for anything.
    #[must_use]
    pub fn snapshot(&self) -> SyncHealth {
        self.0.borrow().clone()
    }

    /// The same snapshot for a consumer that polls rather than waits, such as
    /// a status route.
    #[must_use]
    pub fn receiver(&self) -> watch::Receiver<SyncHealth> {
        self.0.clone()
    }

    /// The first published snapshot `reached` accepts, or the one already
    /// published when it accepts that.
    async fn reached(&mut self, reached: fn(&SyncHealth) -> bool) -> Option<SyncHealth> {
        self.0
            .wait_for(|health| reached(health))
            .await
            .ok()
            .map(|health| SyncHealth::clone(&health))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChainName, NetworkState};

    fn network(chain: &str) -> Network {
        Network {
            chain: ChainName::new(chain),
            name: "regtest".into(),
        }
    }

    fn event(network: &Network, state: NetworkState) -> NetworkEvent {
        NetworkEvent {
            previous: NetworkState::CatchingUp,
            health: NetworkHealth {
                state,
                ..NetworkHealth::starting(network)
            },
        }
    }

    #[tokio::test]
    async fn readiness_waits_for_every_network_the_report_watches() {
        let (first, second) = (network("first"), network("second"));
        let (report, mut readiness) =
            SyncReport::watching(&[first.clone(), second.clone()]);
        report.network_changed(&event(&first, NetworkState::Ready));
        assert!(!readiness.snapshot().ready(), "one network is mid-flight");

        let waiting = tokio::spawn(async move { readiness.ready().await });
        report.network_changed(&event(&second, NetworkState::Ready));

        let reached = waiting.await.expect("waiter").expect("a published snapshot");
        assert!(reached.ready());
        assert!(reached.settled());
    }

    #[tokio::test]
    async fn a_quarantined_network_settles_without_being_ready() {
        let only = network("only");
        let (report, mut readiness) = SyncReport::watching(std::slice::from_ref(&only));

        report.network_changed(&event(&only, NetworkState::Quarantined));

        assert!(
            readiness.settled().await.is_some(),
            "a quarantined network settles: it will not heal by itself"
        );
        assert!(
            !readiness.snapshot().ready(),
            "nothing useful can be served when every network is quarantined"
        );
    }

    #[tokio::test]
    async fn a_reset_report_is_not_ready_again() {
        let only = network("only");
        let (report, readiness) = SyncReport::watching(std::slice::from_ref(&only));
        report.network_changed(&event(&only, NetworkState::Ready));
        assert!(readiness.snapshot().ready());

        report.reset();

        assert!(!readiness.snapshot().ready());
        assert!(!readiness.snapshot().settled());
    }

    #[tokio::test]
    async fn waiting_on_a_dropped_report_ends_rather_than_hangs() {
        let (report, mut readiness) = SyncReport::watching(&[network("only")]);

        drop(report);

        assert!(readiness.ready().await.is_none());
        assert!(readiness.settled().await.is_none());
    }
}
