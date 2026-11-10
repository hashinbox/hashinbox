//! [`Topic`]: what a job locks.

use std::fmt;

/// What a job locks while unfinished: dot-separated segments, broadest
/// first (`a`, `a.b`, `a.b.c`). Two topics conflict when one is the other
/// or a prefix of it at a dot: `a` locks every `a.…`; `a.b` and `a.c` do not
/// lock each other. What the segments mean is the caller's.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Topic(String);

impl Topic {
    /// The topic of `segments`; `None` when there are none, or one is empty
    /// or holds a dot.
    pub fn new<S: AsRef<str>>(segments: impl IntoIterator<Item = S>) -> Option<Self> {
        let segments: Vec<String> = segments
            .into_iter()
            .map(|segment| segment.as_ref().to_owned())
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
        let covers = |broad: &str, narrow: &str| {
            narrow
                .strip_prefix(broad)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        };
        covers(&self.0, &other.0) || covers(&other.0, &self.0)
    }

    /// The topic as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
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
        assert_eq!(Topic::parse("a..b"), None);
        assert_eq!(Topic::new(Vec::<&str>::new()), None);
    }
}
