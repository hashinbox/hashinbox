//! Turns supervised network events into the process's diagnostics, on their
//! way to the health snapshot the HTTP layer serves.

use std::sync::Arc;

use indexing::{IndexError, NetworkEvent, SyncMonitor, SyncReport};

/// The one place a network transition becomes operator-visible: one log line,
/// then the snapshot `indexing::SyncReport` keeps, both non-blocking, as the
/// monitor contract requires.
///
/// This is also the extension point for metrics. The workspace has no metrics
/// client yet; when it gets one it is one more line here, and nothing in
/// `indexing` changes.
pub(crate) struct Health {
    report: Arc<SyncReport>,
}

impl Health {
    pub(crate) const fn new(report: Arc<SyncReport>) -> Self {
        Self { report }
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
        let owed = health.status.backfill.as_ref();
        owed.map_or_else(String::new, |owed| {
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
    fn network_observed(&self, health: &indexing::NetworkHealth) {
        self.report.network_observed(health);
    }

    fn network_changed(&self, event: &NetworkEvent) {
        let health = &event.health;
        eprintln!(
            "sync {}/{}: {:?} -> {:?}{}{}",
            health.status.network.chain,
            health.status.network.name,
            event.previous,
            health.state,
            Self::reason(health, health.failure.as_ref()),
            Self::owed(health),
        );
        self.report.network_changed(event);
    }
}

#[cfg(test)]
mod tests {
    use indexing::{
        ChainName, IndexError, IndexErrorKind, Network, NetworkHealth, NetworkState, SyncReport,
    };

    use super::*;

    #[tokio::test]
    async fn a_failed_attempt_reaches_the_snapshot_with_its_reason() {
        let only = Network {
            chain: ChainName::new("only"),
            name: "regtest".into(),
        };
        let (report, readiness) = SyncReport::watching(std::slice::from_ref(&only));
        let health = Health::new(report);

        health.network_changed(&NetworkEvent {
            previous: NetworkState::Ready,
            health: NetworkHealth {
                state: NetworkState::Retrying,
                failure: Some(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction",
                    true,
                )),
                attempts: 1,
                ..NetworkHealth::starting(&only)
            },
        });

        let reported = readiness.receiver().borrow().networks()[0].clone();
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
