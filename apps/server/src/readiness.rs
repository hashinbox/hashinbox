//! Turns supervised network events into the process's diagnostics and the
//! health snapshot the HTTP layer serves.

use indexing::SyncHealth;
use indexing::{IndexError, NetworkEvent, SyncMonitor};
use tokio::sync::watch;

/// The one place a network transition becomes operator-visible: one log line
/// and one snapshot update, both non-blocking, as the monitor contract requires.
///
/// This is also the extension point for metrics. The workspace has no metrics
/// client yet; when it gets one it is one more line here, and nothing in
/// `indexing` changes.
pub(crate) struct Health {
    snapshot: watch::Sender<SyncHealth>,
}

impl Health {
    pub(crate) const fn new(snapshot: watch::Sender<SyncHealth>) -> Self {
        Self { snapshot }
    }

    fn reason(health: &indexing::NetworkHealth, failure: Option<&IndexError>) -> String {
        failure.map_or_else(String::new, |failure| {
            format!(
                " ({:?}, attempt {}): {}",
                failure.kind, health.attempts, failure.message
            )
        })
    }

    /// The historical range this network still owes, logged because it is the
    /// one condition that no longer shows up as an outage: readiness follows
    /// forward indexing, so a range a pruned node can never serve would
    /// otherwise be silent (`docs/design/024` section 7).
    fn owed(health: &indexing::NetworkHealth) -> String {
        health.backfill.as_ref().map_or_else(String::new, |owed| {
            format!(
                " [backfill {}..={} for {} address(es){}]",
                owed.resume.0,
                owed.anchor.0,
                owed.pending,
                Self::reason(health, owed.failure.as_ref()),
            )
        })
    }
}

impl SyncMonitor for Health {
    fn network_changed(&self, event: &NetworkEvent) {
        let health = &event.health;
        eprintln!(
            "sync {}/{}: {:?} -> {:?}{}{}",
            health.network.chain,
            health.network.name,
            event.previous,
            health.state,
            Self::reason(health, health.failure.as_ref()),
            Self::owed(health),
        );
        self.snapshot.send_modify(|snapshot| snapshot.apply(health));
    }
}

#[cfg(test)]
mod tests {
    use indexing::{
        ChainName, IndexError, IndexErrorKind, Network, NetworkHealth, NetworkState, SyncHealth,
    };

    use super::*;

    fn network(chain: &str) -> Network {
        Network {
            chain: ChainName::new(chain),
            name: "regtest".into(),
        }
    }

    fn event(network: &Network, previous: NetworkState, state: NetworkState) -> NetworkEvent {
        let failure =
            matches!(state, NetworkState::Retrying | NetworkState::Quarantined).then(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction",
                    state == NetworkState::Retrying,
                )
            });
        NetworkEvent {
            previous,
            health: NetworkHealth {
                state,
                failure,
                attempts: 1,
                ..NetworkHealth::starting(network)
            },
        }
    }

    #[tokio::test]
    async fn readiness_follows_every_network_and_survives_one_quarantine() {
        let first = network("first");
        let second = network("second");
        let (snapshot, readiness) =
            watch::channel(SyncHealth::starting(&[first.clone(), second.clone()]));
        let health = Health::new(snapshot);
        assert!(!readiness.borrow().ready());
        assert!(!readiness.borrow().settled());

        health.network_changed(&event(
            &first,
            NetworkState::CatchingUp,
            NetworkState::Ready,
        ));
        assert!(
            !readiness.borrow().ready(),
            "one network is still catching up"
        );

        health.network_changed(&event(
            &second,
            NetworkState::CatchingUp,
            NetworkState::Quarantined,
        ));
        assert!(readiness.borrow().settled(), "startup no longer waits");
        assert!(
            readiness.borrow().ready(),
            "a quarantined network must not hold readiness at false forever"
        );

        health.network_changed(&event(
            &first,
            NetworkState::Ready,
            NetworkState::Quarantined,
        ));
        assert!(!readiness.borrow().ready(), "nothing useful can be served");
    }

    #[tokio::test]
    async fn a_failed_attempt_reaches_the_snapshot_with_its_reason() {
        let only = network("only");
        let (snapshot, readiness) =
            watch::channel(SyncHealth::starting(std::slice::from_ref(&only)));
        let health = Health::new(snapshot);

        health.network_changed(&event(&only, NetworkState::Ready, NetworkState::Retrying));

        let reported = readiness.borrow().networks()[0].clone();
        assert_eq!(reported.state, NetworkState::Retrying);
        assert_eq!(reported.attempts, 1);
        assert_eq!(
            reported
                .failure
                .expect("a retrying network reports why")
                .kind,
            IndexErrorKind::InvalidBlock
        );
    }
}
