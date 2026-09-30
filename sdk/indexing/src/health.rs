//! What indexing reports about itself: `NetworkState` (one network's standing
//! condition), `NetworkHealth` (that state with its checkpoint and failure)
//! and `SyncHealth` (every network at once). Cached values a reader polls,
//! not the result of one pass -- that is `SyncPhase`.

use std::collections::BTreeSet;

use crate::{IndexError, Network, SyncPhase, SyncStatus};

/// What one network's indexing is doing.
///
/// Cached, unlike [`SyncPhase`](crate::SyncPhase): a network's failure no
/// longer ends the process, so the failure has to live somewhere. `SyncPhase`
/// remains the result of one pass; this is the network's standing condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkState {
    /// Behind the observed tip and still applying blocks.
    CatchingUp,
    /// The checkpoint is at the observed tip.
    Ready,
    /// The last pass failed retryably; the next one is waited for.
    Retrying,
    /// The last pass failed terminally; nothing is retried until a resume.
    Quarantined,
}

/// One network's current indexing health: its standing condition, and what the
/// last pass that completed reported.
///
/// A [`SyncStatus`] is what one pass returned; this is what a caller reads
/// between passes, so it holds that status whole rather than restating it, and
/// adds the four facts no pass can report about itself. `status.phase` is the
/// answer of that pass and nothing later: `state` is what the network is doing
/// now, which is why it has two variants [`SyncPhase`] does not, and the two
/// disagree whenever a pass has failed since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkHealth {
    /// What the network is doing.
    pub state: NetworkState,
    /// The error that is holding *forward* indexing back; `None` while it is
    /// healthy. Its `kind`, `retryable` and `message` are the whole of what a
    /// probe or a status route reports.
    ///
    /// A backfill failure never appears here. It is not why the network is in
    /// its state — the state is about the tip, and the tip is fine — so it
    /// travels on the status's [`SyncStatus::backfill`] with the range it
    /// belongs to.
    pub failure: Option<IndexError>,
    /// Consecutive attempts that got nowhere: failed passes, and passes whose
    /// backfill failed. `0` while nothing is failing.
    pub attempts: u32,
    /// Unix seconds at which `state` was entered. Supplied by the driving
    /// loop: this crate owns no clock.
    pub since: u64,
    /// What the last completed pass reported: the network, its confirmed line,
    /// the newest block held above it, the tip it indexed towards and the
    /// history it still owes. Before the first pass it names the network and
    /// nothing else.
    ///
    /// A failed pass never rewrites it, so the checkpoint stays frozen where
    /// indexing got to while the network is retrying or quarantined.
    ///
    /// The owed backfill is reported rather than gating, and this is the whole
    /// of the availability decision: a range a pruned node can never serve
    /// would otherwise hold every chain's readiness down for the life of the
    /// process, while forward indexing answered every question a client
    /// actually asks. An operator reads the stuck range here; a caller reads
    /// the same fact as [`Completeness`](crate::Completeness) on the balance
    /// or the history page it weakens.
    pub status: SyncStatus,
}

/// Every configured network's health and the readiness it implies.
///
/// The collection owns the readiness rule, so a probe, a status route, and a
/// startup gate cannot disagree about what "ready" means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncHealth {
    networks: Vec<NetworkHealth>,
    settled: BTreeSet<Network>,
}

impl NetworkHealth {
    /// A network that has not reported yet: `CatchingUp`, no checkpoint.
    #[must_use]
    pub fn starting(network: &Network) -> Self {
        Self {
            state: NetworkState::CatchingUp,
            failure: None,
            attempts: 0,
            since: 0,
            status: SyncStatus {
                network: network.clone(),
                checkpoint: None,
                pending_tip: None,
                observed_tip: None,
                phase: SyncPhase::CatchingUp,
                unmatched_spends: Vec::new(),
                backfill: None,
            },
        }
    }
}

impl SyncHealth {
    /// Every network of `networks`, in that order, none of them reported yet.
    #[must_use]
    pub fn starting(networks: &[Network]) -> Self {
        Self {
            networks: networks.iter().map(NetworkHealth::starting).collect(),
            settled: BTreeSet::new(),
        }
    }

    /// Replaces one network's row, keeping the configured order.
    ///
    /// A network that is not configured is ignored: the snapshot describes the
    /// composition it was built from, and nothing else.
    pub fn apply(&mut self, health: &NetworkHealth) {
        let Some(row) = self
            .networks
            .iter_mut()
            .find(|row| row.status.network == health.status.network)
        else {
            return;
        };
        row.clone_from(health);
        if matches!(
            health.state,
            NetworkState::Ready | NetworkState::Quarantined
        ) {
            self.settled.insert(health.status.network.clone());
        }
    }

    /// Every configured network, in composition order.
    #[must_use]
    pub fn networks(&self) -> &[NetworkHealth] {
        &self.networks
    }

    /// Whether the process can serve complete history for the chains it still
    /// answers for: no network is mid-flight, and at least one is `Ready`.
    ///
    /// A quarantined network does not hold readiness down, because it will not
    /// recover on its own and the healthy chains still work; its state is
    /// reported per network instead. Every network quarantined is not ready.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.networks
            .iter()
            .all(|row| matches!(row.state, NetworkState::Ready | NetworkState::Quarantined))
            && self
                .networks
                .iter()
                .any(|row| row.state == NetworkState::Ready)
    }

    /// Whether every network has settled at some point: reached `Ready` at least
    /// once, or become `Quarantined`. This is the gate a startup sequence waits
    /// on; `Retrying` is expected to heal and keeps holding it.
    ///
    /// **A latch, not a predicate over the current states.** [`Self::apply`]
    /// records a network the first time it reports `Ready` or `Quarantined` and
    /// nothing ever clears that record, so a network that settled and then fell
    /// behind again still counts here. What the networks are doing *now* is
    /// [`Self::ready`].
    #[must_use]
    pub fn ever_settled(&self) -> bool {
        self.settled.len() == self.networks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChainName;

    fn network(chain: &str) -> Network {
        Network {
            chain: ChainName::new(chain),
            name: "testnet".into(),
        }
    }

    fn reported(network: &Network, state: NetworkState) -> NetworkHealth {
        NetworkHealth {
            state,
            ..NetworkHealth::starting(network)
        }
    }

    #[test]
    fn readiness_needs_every_network_settled_and_at_least_one_ready() {
        let first = network("first");
        let second = network("second");
        let networks = [first.clone(), second.clone()];
        for (states, ready) in [
            ([NetworkState::Ready, NetworkState::Ready], true),
            ([NetworkState::Ready, NetworkState::Quarantined], true),
            (
                [NetworkState::Quarantined, NetworkState::Quarantined],
                false,
            ),
            ([NetworkState::Ready, NetworkState::CatchingUp], false),
            ([NetworkState::Ready, NetworkState::Retrying], false),
        ] {
            let mut health = SyncHealth::starting(&networks);
            health.apply(&reported(&first, states[0]));
            health.apply(&reported(&second, states[1]));
            assert_eq!(health.ready(), ready, "{states:?}");
            assert!(
                health.ever_settled()
                    || states.contains(&NetworkState::CatchingUp)
                    || states.contains(&NetworkState::Retrying)
            );
        }
    }

    #[test]
    fn a_network_stays_settled_after_it_falls_behind_again() {
        let only = network("only");
        let mut health = SyncHealth::starting(std::slice::from_ref(&only));
        assert!(!health.ever_settled());

        health.apply(&reported(&only, NetworkState::Ready));
        assert!(health.ever_settled());

        health.apply(&reported(&only, NetworkState::CatchingUp));
        assert!(health.ever_settled(), "settling is reached once, not held");
        assert!(!health.ready());
    }

    #[test]
    fn an_unconfigured_network_never_enters_the_snapshot() {
        let only = network("only");
        let mut health = SyncHealth::starting(std::slice::from_ref(&only));
        health.apply(&reported(&network("other"), NetworkState::Ready));

        assert_eq!(health.networks(), [NetworkHealth::starting(&only)]);
        assert!(!health.ever_settled());
    }
}
