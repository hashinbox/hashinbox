//! The queue's nouns: a task to enqueue, a claimed one, and its stored result.

use std::{fmt, time::Duration};

/// A task identity: a UUIDv7, so ids order by creation within a lane.
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

/// How often a task is tried and how long a retry waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Claims in all, the first included; at least one.
    pub max_attempts: u32,
    /// A retry after attempt `n` waits `backoff * n`.
    pub backoff: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: Duration::from_millis(250),
        }
    }
}

/// One unit of work to enqueue. The payload is opaque; `kind` selects the executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTask {
    /// One ordering: a lane runs one task at a time, oldest first (e.g. one sender).
    pub lane: String,
    /// The executor selector.
    pub kind: String,
    /// What the executor receives.
    pub payload: Vec<u8>,
    /// Milliseconds since the Unix epoch before which it is not claimed.
    pub run_at: u64,
    /// Milliseconds since the Unix epoch at or after which it expires unrun.
    pub deadline: Option<u64>,
    /// Attempts and backoff.
    pub policy: Policy,
}

impl NewTask {
    /// A task due at once, without deadline, under the default policy.
    #[must_use]
    pub fn new(lane: impl Into<String>, kind: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            lane: lane.into(),
            kind: kind.into(),
            payload,
            run_at: 0,
            deadline: None,
            policy: Policy::default(),
        }
    }
}

/// A task handed to one worker under a lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    /// The task.
    pub id: Id,
    /// Its lane.
    pub lane: String,
    /// Its executor selector.
    pub kind: String,
    /// Its payload.
    pub payload: Vec<u8>,
    /// This claim's attempt number, from 1.
    pub attempt: u32,
}

/// How a task ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The executor finished it.
    Succeeded,
    /// The executor refused it for good (e.g. a node rejected it).
    Rejected,
    /// Attempts were exhausted.
    Failed,
    /// The executor cannot say whether it took effect.
    Ambiguous,
    /// Its deadline passed before it finished.
    Expired,
    /// Cancelled while pending.
    Cancelled,
}

impl Outcome {
    /// The stored code (`schema/queue.sql`).
    #[must_use]
    pub fn code(self) -> i64 {
        match self {
            Self::Succeeded => 0,
            Self::Rejected => 1,
            Self::Failed => 2,
            Self::Ambiguous => 3,
            Self::Expired => 4,
            Self::Cancelled => 5,
        }
    }

    /// The outcome of a stored code.
    #[must_use]
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Succeeded,
            1 => Self::Rejected,
            2 => Self::Failed,
            3 => Self::Ambiguous,
            4 => Self::Expired,
            5 => Self::Cancelled,
            _ => return None,
        })
    }
}

/// A terminal task, written once and kept as the archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskResult {
    /// The task.
    pub task: Id,
    /// How it ended.
    pub outcome: Outcome,
    /// The executor's output on success.
    pub output: Option<Vec<u8>>,
    /// Why it did not succeed.
    pub error: Option<String>,
    /// Claims it took.
    pub attempts: u32,
    /// Milliseconds since the Unix epoch.
    pub finished_at: u64,
}

/// Where a task is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Waiting to be claimed.
    Pending,
    /// Claimed under a lease.
    Running,
    /// Terminal.
    Finished(TaskResult),
}

/// A worker's settlement of its claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// Terminal, with `outcome`.
    Finish {
        /// How it ended.
        outcome: Outcome,
        /// Output on success.
        output: Option<Vec<u8>>,
        /// Why it did not succeed.
        error: Option<String>,
    },
    /// Run again after the backoff if attempts remain, else `Failed`.
    Retry {
        /// Why this attempt failed.
        error: String,
    },
}
