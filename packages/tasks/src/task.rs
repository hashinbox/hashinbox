//! The queue's nouns: a job and where it is.

use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

/// A job identity: a UUIDv7, so ids order jobs by creation.
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

/// What a job locks while unfinished: dot-separated segments, broadest
/// first (`a`, `a.b`, `a.b.c`). Two topics conflict when one covers the
/// other: matches a prefix of it at a dot, where `*` matches one or more
/// segments. `a` locks every `a.…`; `a.b` and `a.c` do not lock each other;
/// `*.w` locks `n.w` and `n.x.w`. What the segments mean is the caller's.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Topic(String);

/// Any one or more segments.
const ANY: &str = "*";

impl Topic {
    /// The topic of `segments`; `None` when there are none, or one is empty
    /// or holds a dot.
    pub fn new<S: AsRef<str>>(segments: impl IntoIterator<Item = S>) -> Option<Self> {
        let segments: Vec<String> = segments
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect();
        let valid = !segments.is_empty()
            && segments
                .iter()
                .all(|segment| !segment.is_empty() && !segment.contains('.'));
        valid.then(|| Self(segments.join(".")))
    }

    /// A stored topic (`a.b.c`); `None` when it is not one.
    #[must_use]
    pub fn parse(topic: &str) -> Option<Self> {
        Self::new(topic.split('.'))
    }

    /// Whether this and `other` may not run at once.
    #[must_use]
    pub fn conflicts(&self, other: &Self) -> bool {
        let (mine, theirs) = (self.segments(), other.segments());
        covers(&mine, &theirs) || covers(&theirs, &mine)
    }

    /// The topic as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn segments(&self) -> Vec<&str> {
        self.0.split('.').collect()
    }
}

/// Whether `pattern` matches a prefix of `topic`: a `*` in `pattern` takes
/// one or more segments, a `*` in `topic` matches any one segment.
fn covers(pattern: &[&str], topic: &[&str]) -> bool {
    match (pattern.split_first(), topic.split_first()) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some((&ANY, rest)), Some((_, after))) => {
            (0..=after.len()).any(|taken| covers(rest, &after[taken..]))
        }
        (Some((first, rest)), Some((head, after))) => {
            (first == head || *head == ANY) && covers(rest, after)
        }
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
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
}

impl Status {
    /// Whether it is terminal.
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// One job, as stored. Queue fields are the queue's; `input` and `state` are
/// its worker's.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// The job; ids order jobs.
    pub id: Id,
    /// What it locks while unfinished.
    pub topic: Topic,
    /// Which worker runs it.
    pub kind: String,
    /// Where it is.
    pub status: Status,
    /// Milliseconds since the Unix epoch before which it does not start.
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
    /// A pending job of `kind` on `topic`, runnable now.
    #[must_use]
    pub fn new(topic: Topic, kind: impl Into<String>, input: Value) -> Self {
        Self {
            id: Id::new(),
            topic,
            kind: kind.into(),
            status: Status::Pending,
            run_at: 0,
            deadline: None,
            attempts: 0,
            reason: None,
            input,
            state: None,
        }
    }

    /// The same, not started before `run_at`.
    #[must_use]
    pub fn run_at(mut self, run_at: u64) -> Self {
        self.run_at = run_at;
        self
    }

    /// The same, not started after `deadline`.
    #[must_use]
    pub fn with_deadline(mut self, deadline: u64) -> Self {
        self.deadline = Some(deadline);
        self
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

#[cfg(test)]
mod tests {
    use super::Topic;

    fn topic(text: &str) -> Topic {
        Topic::parse(text).unwrap()
    }

    #[test]
    fn a_topic_conflicts_with_itself_and_what_it_covers() {
        assert!(topic("n").conflicts(&topic("n.w1")));
        assert!(topic("n.w1").conflicts(&topic("n")));
        assert!(topic("n.w1").conflicts(&topic("n.w1")));
        assert!(!topic("n.w1").conflicts(&topic("n.w2")));
        assert!(!topic("n").conflicts(&topic("nn.w1")));
        assert!(!topic("m").conflicts(&topic("n")));
        assert!(topic("*.w1").conflicts(&topic("n.w1")));
        assert!(topic("*.w1").conflicts(&topic("n.x.w1")));
        assert!(topic("n.x.w1").conflicts(&topic("*.w1")));
        assert!(!topic("*.w1").conflicts(&topic("n.w2")));
        assert!(topic("n.*").conflicts(&topic("n.w1.a")));
        assert_eq!(Topic::parse("a..b"), None);
        assert_eq!(Topic::new(Vec::<&str>::new()), None);
    }
}
