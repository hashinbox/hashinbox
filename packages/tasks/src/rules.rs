//! The state machine on [`Job`]: starting it and turning a worker's answer
//! into its next state.

use std::{num::NonZeroU32, time::Duration};

use crate::task::{Job, Status};

/// How a worker answered a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ack {
    /// Succeeded.
    Done,
    /// Failed for good, with why.
    Fail(String),
    /// Run again after the backoff while attempts remain, with why not now.
    Retry(String),
    /// Succeeded for now; run again at this Unix ms, as fresh: attempts and
    /// reason cleared. A job that never ends (a poller) answers this.
    Again(u64),
}

/// How retries go: at most `attempts` executions, a retry after attempt `n`
/// waiting `backoff × n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Executions, the first included.
    pub attempts: NonZeroU32,
    /// The wait per attempt made.
    pub backoff: Duration,
}

impl Default for Retry {
    /// Three attempts, 250 ms backoff.
    fn default() -> Self {
        Self {
            attempts: NonZeroU32::new(3).unwrap_or(NonZeroU32::MIN),
            backoff: Duration::from_millis(250),
        }
    }
}

impl Job {
    /// This job taken from `next` at `now`: running with its attempt
    /// counted, or failed `expired` past its deadline.
    #[must_use]
    pub fn start(mut self, now: u64) -> Self {
        if self.deadline.is_some_and(|deadline| deadline < now) {
            self.status = Status::Failed;
            self.reason = Some("expired".into());
            return self;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.status = Status::Running;
        self
    }

    /// This job after `ack` at `now`.
    #[must_use]
    pub fn settle(mut self, ack: Ack, retry: &Retry, now: u64) -> Self {
        match ack {
            Ack::Done => self.status = Status::Done,
            Ack::Fail(reason) => {
                self.status = Status::Failed;
                self.reason = Some(reason);
            }
            Ack::Retry(reason) if self.attempts >= retry.attempts.get() => {
                self.status = Status::Failed;
                self.reason = Some(format!("attempts-exhausted: {reason}"));
            }
            Ack::Again(at) => {
                self.status = Status::Pending;
                self.run_at = at;
                self.attempts = 0;
                self.reason = None;
            }
            Ack::Retry(reason) => {
                let wait = retry.backoff.saturating_mul(self.attempts);
                let wait = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
                self.status = Status::Pending;
                self.run_at = now.saturating_add(wait);
                self.reason = Some(reason);
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::task::Topic;

    fn retry() -> Retry {
        Retry {
            attempts: NonZeroU32::new(2).unwrap(),
            backoff: Duration::from_millis(10),
        }
    }

    #[test]
    fn start_counts_the_attempt_or_expires() {
        let job = Job::new(Topic::parse("a").unwrap(), "k", json!(null)).with_deadline(5);
        let started = job.clone().start(5);
        assert_eq!((started.status, started.attempts), (Status::Running, 1));
        let expired = job.start(6);
        assert_eq!(expired.status, Status::Failed);
        assert_eq!(expired.reason.as_deref(), Some("expired"));
        assert_eq!(expired.attempts, 0);
    }

    #[test]
    fn settle_follows_the_table() {
        let running = Job::new(Topic::parse("a").unwrap(), "k", json!(null)).start(0);
        assert_eq!(
            running.clone().settle(Ack::Done, &retry(), 0).status,
            Status::Done
        );
        let failed = running.clone().settle(Ack::Fail("no".into()), &retry(), 0);
        assert_eq!(
            (failed.status, failed.reason.as_deref()),
            (Status::Failed, Some("no"))
        );
        let again = running.settle(Ack::Retry("busy".into()), &retry(), 100);
        assert_eq!((again.status, again.run_at), (Status::Pending, 110));
        let polled = again
            .clone()
            .start(110)
            .settle(Ack::Again(500), &retry(), 120);
        assert_eq!(
            (polled.status, polled.run_at, polled.attempts, polled.reason),
            (Status::Pending, 500, 0, None)
        );
        let exhausted = again
            .start(110)
            .settle(Ack::Retry("busy".into()), &retry(), 120);
        assert_eq!(exhausted.status, Status::Failed);
        assert_eq!(
            exhausted.reason.as_deref(),
            Some("attempts-exhausted: busy")
        );
    }
}
