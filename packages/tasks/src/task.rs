//! The queue's nouns: a job and where it is.

use std::fmt;

/// A job identity: a UUIDv7, so ids order by creation within a lane.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(String);

impl Id {
    /// A fresh, time-ordered id.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    /// The id as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl From<String> for Id {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for its `run_at` and its lane.
    Pending,
    /// Executing; its lane waits for it.
    Running,
    /// Succeeded.
    Done,
    /// Failed for good; the payload's `reason` says why.
    Failed,
}

impl State {
    /// Whether it is terminal.
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// One job, as stored. The store keeps it whole; every rule is the runner's.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// The job; ids order a lane.
    pub id: Id,
    /// One ordering: a lane runs one job at a time, oldest first (e.g. one sender).
    pub lane: String,
    /// The executor selector.
    pub kind: String,
    /// Where it is.
    pub state: State,
    /// Its input and whatever its executor and the runner save (`attempts`,
    /// `reason`, an optional `deadline` in ms); replaced whole on every set.
    pub payload: serde_json::Value,
    /// Milliseconds since the Unix epoch before which it does not run.
    pub run_at: u64,
}

impl Job {
    /// A pending job due at once.
    #[must_use]
    pub fn new(
        lane: impl Into<String>,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id: Id::new(),
            lane: lane.into(),
            kind: kind.into(),
            state: State::Pending,
            payload,
            run_at: 0,
        }
    }
}
