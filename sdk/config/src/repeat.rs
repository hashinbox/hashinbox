use std::{num::NonZeroU32, sync::Arc, time::Duration};

use serde::Deserialize;

use crate::{Error, ErrorKind};

/// How much the transport repeats a call: `"off"` for no repetition at all,
/// or an explicit [`RepetitionPolicy`]. Absent from a document, the
/// transport's own default applies, which is what [`Repetition::default`]
/// restates.
///
/// Repetition is a property of the endpoint *list*, not of one node
/// (`a retired design note (023)` section 3.4), so it is a sibling of
/// [`Rpc::endpoints`](crate::Rpc::endpoints) rather than a URL parameter.
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
    #[serde(rename = "backoff_ms", deserialize_with = "crate::millis::deserialize")]
    pub backoff: Duration,
    /// The ceiling that doubling never passes.
    #[serde(
        rename = "max_backoff_ms",
        deserialize_with = "crate::millis::deserialize"
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::Rpc;

    fn repeat(value: Value) -> Result<Repetition, Error> {
        let rpc: Rpc = serde_json::from_value(
            json!({"endpoints": ["http://127.0.0.1:8545"], "repeat": value}),
        )
        .map_err(|error| Error::new(ErrorKind::Repeat, error.to_string()))?;
        rpc.validate()?;
        Ok(rpc.repeat)
    }

    #[test]
    fn repetition_defaults_to_the_transport_policy_and_reads_off_or_an_explicit_one() {
        let absent: Rpc = serde_json::from_value(json!({"endpoints": ["http://127.0.0.1:8545"]}))
            .expect("repeat may be omitted");
        let Repetition::Policy(policy) = absent.repeat else {
            panic!("the default is a policy");
        };
        assert_eq!(
            (policy.attempts.get(), policy.backoff, policy.max_backoff),
            (3, Duration::from_millis(250), Duration::from_secs(2))
        );
        assert_eq!(
            policy,
            RepetitionPolicy::new(),
            "one set of numbers, one place"
        );

        let off = repeat(json!("off")).expect("`off` is a policy");
        assert!(matches!(off, Repetition::Off));
        assert!(off.validate().is_ok());

        let Repetition::Policy(policy) =
            repeat(json!({"attempts": 7, "backoff_ms": 10, "max_backoff_ms": 20}))
                .expect("an explicit policy")
        else {
            panic!("an explicit policy");
        };
        assert_eq!(policy.attempts.get(), 7);
        assert_eq!(policy.backoff, Duration::from_millis(10));
    }

    #[test]
    fn the_repeat_wire_contract_is_closed_and_its_bounds_are_ordered() {
        for value in [
            json!("on"),
            json!(3),
            json!({}),
            json!({"attempts": 0, "backoff_ms": 1, "max_backoff_ms": 2}),
            json!({"attempts": 3, "backoff_ms": 1}),
            json!({"attempts": 3, "backoff_ms": 1, "max_backoff_ms": 2, "jitter_ms": 1}),
            json!({"attempts": 3, "backoff": 1, "max_backoff": 2}),
        ] {
            assert!(repeat(value.clone()).is_err(), "{value} must be rejected");
        }
        let error = repeat(json!({"attempts": 3, "backoff_ms": 2_000, "max_backoff_ms": 250}))
            .expect_err("a backoff above its cap is a typo");
        assert_eq!(
            error.to_string(),
            "RPC retry backoff must not exceed its maximum"
        );
        assert_eq!(error.kind, ErrorKind::Repeat);
    }

    /// The bound a `deserialize_with` would have hidden: a policy assembled in
    /// Rust reaches the same rejection as one read from a document.
    #[test]
    fn an_inverted_policy_built_in_code_is_rejected_the_same_way() {
        let inverted = Repetition::Policy(RepetitionPolicy {
            backoff: Duration::from_secs(2),
            max_backoff: Duration::from_millis(250),
            ..RepetitionPolicy::new()
        });
        let coded = inverted.validate().expect_err("an inverted policy");
        let read = repeat(json!({"attempts": 3, "backoff_ms": 2_000, "max_backoff_ms": 250}))
            .expect_err("the same document");
        assert_eq!((coded.kind, coded.message), (read.kind, read.message));
    }
}
