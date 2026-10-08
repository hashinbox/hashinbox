use std::{num::NonZeroU32, sync::Arc, time::Duration};

use serde::Deserialize;

use crate::config::{Error, ErrorKind};

/// How much the transport repeats a call: `"off"` for no repetition at all,
/// or an explicit [`RepetitionPolicy`]. Absent from a document, the
/// transport's own default applies, which is what [`Repetition::default`]
/// restates.
///
/// Repetition is a property of the endpoint *list*, not of one node
/// (`a retired design note (023)` section 3.4), so it is a sibling of
/// [`Rpc::endpoints`](crate::config::Rpc::endpoints) rather than a URL parameter.
///
/// This is a *value*. [`json_rpc::Retry`] is the *decorator* it configures,
/// and the two used to share the name `Retry` — one word for a number an
/// operator writes and for a client wrapper (`docs/DEFECT_PATTERNS.md` 2.3).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Repetition {
    /// One round over the endpoint list, and no more.
    Off,
    /// An explicit policy.
    #[serde(untagged)]
    Policy(RepetitionPolicy),
}

impl Repetition {
    /// Rejects a policy whose first wait is longer than its own cap.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Repeat`] when `backoff_ms` exceeds `max_backoff_ms`.
    pub fn validate(&self) -> Result<(), Error> {
        let Self::Policy(policy) = self else {
            return Ok(());
        };
        if policy.backoff > policy.max_backoff {
            return Err(Error::new(
                ErrorKind::Repeat,
                "RPC retry backoff must not exceed its maximum",
            ));
        }
        Ok(())
    }

    /// Wraps `inner` in this repetition, or hands it back untouched when the
    /// deployment asked for none.
    pub fn wrap<C: json_rpc::Client + 'static>(&self, inner: C) -> Arc<dyn json_rpc::Client> {
        match self {
            Self::Off => Arc::new(inner),
            Self::Policy(policy) => Arc::new(policy.wrap(inner)),
        }
    }
}

impl Default for Repetition {
    /// What the transport has always done: three attempts, 250 ms doubling to
    /// a 2 s cap, transport failures only.
    fn default() -> Self {
        Self::Policy(RepetitionPolicy::default())
    }
}

/// How many attempts one call may make, and how long it waits between them.
///
/// The fields are public and [`RepetitionPolicy::new`] fills the transport's
/// own numbers, so a deployment overrides one of them with struct-update
/// syntax and never restates the other two. A document writes the two waits as
/// `backoff_ms` and `max_backoff_ms`; in Rust they are [`Duration`]s, which is
/// what [`json_rpc::Backoff`] takes.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepetitionPolicy {
    /// How many times one call may be sent, including the first.
    pub attempts: NonZeroU32,
    /// The first wait between two attempts; it doubles.
    #[serde(
        rename = "backoff_ms",
        deserialize_with = "crate::config::millis::deserialize"
    )]
    pub backoff: Duration,
    /// The ceiling that doubling never passes.
    #[serde(
        rename = "max_backoff_ms",
        deserialize_with = "crate::config::millis::deserialize"
    )]
    pub max_backoff: Duration,
}

impl RepetitionPolicy {
    /// The transport's own numbers: three attempts, 250 ms doubling to a 2 s
    /// cap.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            attempts: NonZeroU32::new(3).expect("three is nonzero"),
            backoff: Duration::from_millis(250),
            max_backoff: Duration::from_millis(2_000),
        }
    }

    fn wrap<C: json_rpc::Client>(&self, inner: C) -> json_rpc::Retry<C> {
        json_rpc::Retry::new(inner)
            .with_attempts(self.attempts)
            .with_backoff(json_rpc::Backoff::Exponential {
                initial: self.backoff,
                max: self.max_backoff,
            })
    }
}

impl Default for RepetitionPolicy {
    fn default() -> Self {
        Self::new()
    }
}
