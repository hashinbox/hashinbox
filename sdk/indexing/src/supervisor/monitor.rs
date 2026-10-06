use crate::NetworkHealth;

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
/// [`SyncControl::resume`](super::SyncControl::resume).
pub trait SyncMonitor: Send + Sync + 'static {
    /// Samples the latest network status after every completed pass or state update.
    /// Implementations must return promptly and perform no I/O.
    fn network_observed(&self, health: &NetworkHealth);
}
