//! How often a driver polls each network.
//!
//! Outside the `supervisor` feature deliberately: a cadence is configuration a
//! chain declares and an application carries, with no runtime in it at all
//! (`std::time::Duration` and a map). `crate::Supervisor` is one consumer of
//! it; a caller driving `crate::Indexing::sync` from its own loop is another,
//! and neither should have to compile a Tokio task pool to name an interval.

use std::{collections::BTreeMap, time::Duration};

use crate::Network;

/// How often one unit polls, and how far its retry wait may grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// Wait after a pass that reached the observed tip. A unit that is still
    /// catching up never waits.
    pub interval: Duration,
    /// Upper bound on the retry wait, so a chain whose node is down keeps
    /// probing at a fixed, cheap rate instead of doubling forever.
    pub ceiling: Duration,
}

/// Each network's schedule, with one fallback for the networks not named.
///
/// One cadence per chain rather than the shortest of them: a Solana unit
/// polling every 400 ms no longer forces Bitcoin to do the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedules {
    fallback: Schedule,
    networks: BTreeMap<Network, Schedule>,
}

impl Schedule {
    /// How many polling intervals the retry wait may grow to by default.
    const CEILING_INTERVALS: u32 = 60;

    /// Polls every `interval`, capping the retry wait at sixty of them.
    #[must_use]
    pub fn every(interval: Duration) -> Self {
        Self {
            interval,
            ceiling: interval.saturating_mul(Self::CEILING_INTERVALS),
        }
    }

    /// Polls every `interval`, capping the retry wait at `ceiling`. A ceiling
    /// below `interval` is raised to it: a retry never runs sooner than a
    /// healthy poll.
    #[must_use]
    pub fn new(interval: Duration, ceiling: Duration) -> Self {
        Self {
            interval,
            ceiling: ceiling.max(interval),
        }
    }

    /// The wait before retry number `attempts` (the first failed pass is `1`):
    /// the polling interval doubled once per earlier attempt, capped by
    /// [`Self::ceiling`].
    #[must_use]
    pub fn retry(&self, attempts: u32) -> Duration {
        const MAX_DOUBLINGS: u32 = 31;

        let doublings = attempts.saturating_sub(1).min(MAX_DOUBLINGS);
        self.interval
            .saturating_mul(1_u32 << doublings)
            .min(self.ceiling)
    }
}

impl Schedules {
    /// Every network polls every `interval` until one is given its own.
    #[must_use]
    pub fn every(interval: Duration) -> Self {
        Self {
            fallback: Schedule::every(interval),
            networks: BTreeMap::new(),
        }
    }

    /// `network` polls on `schedule` instead of the fallback.
    #[must_use]
    pub fn with(mut self, network: &Network, schedule: Schedule) -> Self {
        self.networks.insert(network.clone(), schedule);
        self
    }

    /// The schedule `network` polls on: its own, or the fallback.
    #[must_use]
    pub fn of(&self, network: &Network) -> Schedule {
        self.networks.get(network).copied().unwrap_or(self.fallback)
    }
}

#[cfg(test)]
mod tests {
    use crate::ChainName;

    use super::*;

    fn network(chain: &str) -> Network {
        Network {
            chain: ChainName::new(chain),
            name: "testnet".into(),
        }
    }

    #[test]
    fn retry_backoff_doubles_per_attempt_and_stops_at_the_ceiling() {
        let schedule = Schedule::new(Duration::from_millis(100), Duration::from_millis(800));
        for (attempts, millis) in [(1, 100), (2, 200), (3, 400), (4, 800), (5, 800), (99, 800)] {
            assert_eq!(
                schedule.retry(attempts),
                Duration::from_millis(millis),
                "attempt {attempts}"
            );
        }
        assert_eq!(
            Schedule::every(Duration::from_secs(1)).ceiling,
            Duration::from_secs(60)
        );
        assert_eq!(
            Schedule::new(Duration::from_secs(5), Duration::from_secs(1)).ceiling,
            Duration::from_secs(5),
            "a retry never runs sooner than a healthy poll"
        );
    }

    #[test]
    fn each_network_keeps_its_own_cadence_and_unnamed_networks_take_the_fallback() {
        let fast = network("fast");
        let slow = network("slow");
        let schedules = Schedules::every(Duration::from_secs(1))
            .with(&fast, Schedule::every(Duration::from_millis(400)));

        assert_eq!(schedules.of(&fast).interval, Duration::from_millis(400));
        assert_eq!(schedules.of(&slow).interval, Duration::from_secs(1));
    }
}
