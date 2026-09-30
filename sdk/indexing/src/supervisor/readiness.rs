use std::sync::Arc;

use tokio::sync::watch;

use super::{NetworkEvent, SyncMonitor};
use crate::{Network, SyncHealth};

/// The monitor that keeps one [`SyncHealth`] snapshot current, and publishes
/// it to the [`Readiness`] a caller waits on.
///
/// Every consumer of a [`Supervisor`](super::Supervisor) needs somewhere for a
/// transition to land and a way to wait until the index has caught up. The
/// rule itself stays where it was: [`SyncHealth`] owns it.
///
/// It does no I/O, as [`SyncMonitor`] requires, and it writes nothing: an
/// application that wants a line per transition implements [`SyncMonitor`]
/// itself and forwards each event here.
pub struct SyncReport {
    snapshot: watch::Sender<SyncHealth>,
    /// What every network looks like before it has reported, kept so
    /// [`Self::reset`] can say exactly that again.
    starting: SyncHealth,
}

/// Waits until every network a [`SyncReport`] watches has settled.
pub struct Readiness(watch::Receiver<SyncHealth>);

impl SyncReport {
    /// A monitor over `networks`, none of them reported yet, and the readiness
    /// it publishes to.
    ///
    /// The networks are the composition's own
    /// ([`Index::networks`](crate::Index::networks)); a transition on any
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

impl SyncMonitor for SyncReport {
    fn network_changed(&self, event: &NetworkEvent) {
        self.snapshot
            .send_modify(|snapshot| snapshot.apply(&event.health));
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
    /// `None` once the [`SyncReport`] is gone: the supervisor that owned it
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
mod tests {
    use super::*;
    use crate::{ChainName, NetworkHealth, NetworkState};

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
    async fn the_waiter_wakes_when_the_last_network_settles() {
        let (first, second) = (network("first"), network("second"));
        let (report, mut readiness) = SyncReport::watching(&[first.clone(), second.clone()]);
        let waiting = tokio::spawn(async move { readiness.settled().await });

        report.network_changed(&event(&first, NetworkState::Ready));
        report.network_changed(&event(&second, NetworkState::Retrying));
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "one network has reached its tip and the other is still trying"
        );

        report.network_changed(&event(&second, NetworkState::Ready));

        let settled = waiting.await.expect("waiter").expect("a live report");
        assert!(settled.ready());
        assert!(
            settled
                .networks()
                .iter()
                .all(|row| row.state == NetworkState::Ready)
        );
    }

    #[tokio::test]
    async fn a_quarantined_network_does_not_hold_the_waiter_back() {
        let (first, second) = (network("first"), network("second"));
        let (report, mut readiness) = SyncReport::watching(&[first.clone(), second.clone()]);
        let waiting = tokio::spawn(async move { readiness.settled().await });

        report.network_changed(&event(&first, NetworkState::Quarantined));
        report.network_changed(&event(&second, NetworkState::Ready));
        tokio::task::yield_now().await;

        assert!(waiting.is_finished(), "a quarantine heals by no waiting");
        let settled = waiting.await.expect("waiter").expect("a live report");
        assert_eq!(settled.networks()[0].state, NetworkState::Quarantined);
        assert!(settled.ready(), "the healthy network still serves");
    }

    #[tokio::test]
    async fn every_network_quarantined_still_settles() {
        let only = network("only");
        let (report, mut readiness) = SyncReport::watching(std::slice::from_ref(&only));
        let waiting = tokio::spawn(async move { readiness.settled().await });

        report.network_changed(&event(&only, NetworkState::Quarantined));
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
        let (report, readiness) = SyncReport::watching(&[network("watched")]);

        report.network_changed(&event(&network("other"), NetworkState::Ready));

        assert!(!readiness.receiver().borrow().ever_settled());
    }

    #[tokio::test]
    async fn a_reset_report_has_to_settle_again() {
        let only = network("only");
        let (report, mut readiness) = SyncReport::watching(std::slice::from_ref(&only));
        report.network_changed(&event(&only, NetworkState::Ready));
        assert!(readiness.settled().await.is_some());

        report.reset();

        let snapshot = readiness.receiver().borrow().clone();
        assert!(!snapshot.ready());
        assert!(!snapshot.ever_settled());
    }

    #[tokio::test]
    async fn waiting_on_a_dropped_report_ends_rather_than_hangs() {
        let (report, mut readiness) = SyncReport::watching(&[network("only")]);

        drop(report);

        assert!(readiness.settled().await.is_none());
    }
}
