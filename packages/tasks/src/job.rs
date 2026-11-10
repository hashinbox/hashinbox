//! [`Job`]: one job as stored, its [`Id`] and its [`Status`].

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use crate::{
    topic::Topic,
    worker::{Ack, Policy},
};

/// A job's identity and its place in line: the store numbers jobs in the
/// order they enter, from 1. `0` until stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u64);

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Waiting for its `run_at` and for earlier jobs of conflicting topics.
    Pending,
    /// Executing; later jobs of conflicting topics wait for it.
    Running,
    /// Succeeded.
    Done,
    /// Failed for good; `reason` says why.
    Failed,
    /// Finished without running; `reason` names the job it gave way to.
    Skipped,
}

impl Status {
    /// Whether it is terminal.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Skipped)
    }
}

/// One job, as stored. Queue fields are the queue's; `input` and `state` are
/// its worker's.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// Its identity and place in line; `0` until stored.
    pub id: Id,
    /// What it locks while unfinished.
    pub topic: Topic,
    /// Which worker runs it.
    pub kind: String,
    /// Where it is.
    pub status: Status,
    /// Milliseconds since the Unix epoch before which it does not start;
    /// while it runs, when its lease ends.
    pub run_at: u64,
    /// Milliseconds since the Unix epoch after which it does not start.
    pub deadline: Option<u64>,
    /// Executions started.
    pub attempts: u32,
    /// Why it failed, or why it last retried.
    pub reason: Option<String>,
    /// What to do: set when enqueued, never changed.
    pub input: Value,
    /// What its worker has done so far; `None` until first saved.
    pub state: Option<Value>,
}

impl Job {
    /// A pending job of `kind` on `topic`, due now.
    #[must_use]
    pub fn new(topic: Topic, kind: impl Into<String>, input: Value) -> Self {
        Self {
            id: Id::default(),
            topic,
            kind: kind.into(),
            status: Status::Pending,
            run_at: now(),
            deadline: None,
            attempts: 0,
            reason: None,
            input,
            state: None,
        }
    }

    /// The same, not started before `run_at`.
    #[must_use]
    pub const fn run_at(mut self, run_at: u64) -> Self {
        self.run_at = run_at;
        self
    }

    /// The same, not started after `deadline`.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: u64) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Whether it may be claimed at `now`: pending or running, its
    /// `run_at` reached (for a running job: its lease ended), its deadline
    /// not passed.
    #[must_use]
    pub fn is_due(&self, now: u64) -> bool {
        matches!(self.status, Status::Pending | Status::Running)
            && self.run_at <= now
            && self.deadline.is_none_or(|deadline| now <= deadline)
    }

    /// Whether it is past its deadline at `now` and not under a lease: it
    /// will never start.
    #[must_use]
    pub fn is_expired(&self, now: u64) -> bool {
        let unleased = self.status == Status::Pending
            || (self.status == Status::Running && self.run_at <= now);
        unleased && self.deadline.is_some_and(|deadline| deadline < now)
    }

    /// Whether it holds its place in line at `now`: running, or pending and
    /// either due or retried before. A job waiting for a fresh run blocks
    /// nobody until it is due.
    #[must_use]
    pub fn holds_place(&self, now: u64) -> bool {
        match self.status {
            Status::Running => true,
            Status::Pending => self.run_at <= now || self.attempts > 0,
            Status::Done | Status::Failed | Status::Skipped => false,
        }
    }
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Job {
    /// This job after `ack` at `now`, by its kind's `policy`.
    #[must_use]
    pub fn settle(mut self, ack: Ack, policy: &Policy, now: u64) -> Self {
        let retry = &policy.retry;
        match ack {
            Ack::Done => self.status = Status::Done,
            Ack::Fail(reason) => return self.fail(reason),
            Ack::Retry(reason) if self.attempts >= retry.attempts.get() => {
                return self.fail(format!("attempts-exhausted: {reason}"));
            }
            Ack::Retry(reason) => {
                let wait = retry.backoff.saturating_mul(self.attempts);
                self.status = Status::Pending;
                self.run_at = now.saturating_add(millis(wait));
                self.reason = Some(reason);
            }
            Ack::Again(at, reason) => return self.again(at, reason),
        }
        self
    }

    /// Finished without running, giving way to `holder`.
    pub(crate) fn skip(mut self, holder: Id) -> Self {
        self.status = Status::Skipped;
        self.reason = Some(format!("skipped: {holder}"));
        self
    }

    /// Failed for good, with why.
    pub(crate) fn fail(mut self, reason: String) -> Self {
        self.status = Status::Failed;
        self.reason = Some(reason);
        self
    }

    /// Pending for a new run at `at`, fresh.
    fn again(mut self, at: u64, reason: Option<String>) -> Self {
        self.status = Status::Pending;
        self.run_at = at;
        self.attempts = 0;
        self.reason = reason;
        self
    }
}

/// `duration` in milliseconds, saturating.
#[must_use]
pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::{num::NonZeroU32, time::Duration};

    use super::*;
    use crate::worker::Retry;

    const POLICY: Policy = Policy {
        retry: Retry {
            attempts: NonZeroU32::new(2).unwrap(),
            backoff: Duration::from_millis(10),
        },
        ..Policy::DEFAULT
    };

    /// A job stored as 7, running its first attempt.
    fn running() -> Job {
        let job = Job::new(Topic::parse("a").unwrap(), "k", json!(null)).run_at(0);
        Job {
            id: Id(7),
            status: Status::Running,
            attempts: 1,
            ..job
        }
    }

    #[test]
    fn settle_follows_the_table() {
        let settle = |ack| running().settle(ack, &POLICY, 100);
        let view = |job: Job| (job.status, job.run_at, job.attempts, job.reason);
        assert_eq!(view(settle(Ack::Done)), (Status::Done, 0, 1, None));
        assert_eq!(
            view(settle(Ack::Fail("no".into()))),
            (Status::Failed, 0, 1, Some("no".into()))
        );
        assert_eq!(
            view(settle(Ack::Retry("busy".into()))),
            (Status::Pending, 110, 1, Some("busy".into()))
        );
        assert_eq!(
            view(settle(Ack::Again(500, None))),
            (Status::Pending, 500, 0, None)
        );
        assert_eq!(
            view(settle(Ack::Again(900, Some("down".into())))),
            (Status::Pending, 900, 0, Some("down".into()))
        );
        let exhausted = Job {
            attempts: 2,
            ..running()
        }
        .settle(Ack::Retry("busy".into()), &POLICY, 120);
        assert_eq!(
            view(exhausted),
            (
                Status::Failed,
                0,
                2,
                Some("attempts-exhausted: busy".into())
            )
        );
    }

    #[test]
    fn a_skip_names_the_holder() {
        let skipped = running().skip(Id(9));
        assert_eq!(
            (skipped.status, skipped.reason.as_deref()),
            (Status::Skipped, Some("skipped: 9"))
        );
        assert!(skipped.status.is_finished());
    }
}
