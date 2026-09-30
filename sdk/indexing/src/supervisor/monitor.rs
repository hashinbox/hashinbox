use crate::{NetworkHealth, NetworkState};

/// Observes network snapshots and receives state-change notifications.
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
/// [`SyncControl::resume`](super::SyncControl::resume).
pub trait SyncMonitor: Send + Sync + 'static {
    /// Samples the latest network status after every completed pass or state update.
    /// This is a snapshot hook, separate from transition notifications. The default
    /// does nothing, so event-only monitors retain their existing cadence.
    /// Implementations must return promptly and perform no I/O.
    fn network_observed(&self, _health: &NetworkHealth) {}

    /// Reports one network's transition, or one more failed attempt on it.
    ///
    /// Fires on every state change and on every failed attempt, and never once
    /// per catch-up batch: a network that stays `CatchingUp` is silent, while
    /// a network retrying at attempt 10 000 stays distinguishable from one at
    /// attempt 2.
    fn network_changed(&self, event: &NetworkEvent);
}

/// One network's transition: the state it left, and everything it is now.
#[derive(Clone, Debug)]
pub struct NetworkEvent {
    /// The state the network left. Equal to `health.state` when only the
    /// attempt count changed.
    pub previous: NetworkState,
    /// The network's health after the transition: its identity, state,
    /// checkpoint, failure, attempt count, and when the state was entered.
    pub health: NetworkHealth,
}
