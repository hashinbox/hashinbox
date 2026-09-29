use std::time::Duration;

use indexing::SyncPolicy;
use serde::Deserialize;

use crate::{Error, ErrorKind};

/// The rhythm one chain indexes at: how often it asks its node for a new tip,
/// and the bounds one synchronization pass runs under.
///
/// This used to be called `Sync`, which shadowed [`std::marker::Sync`] — the
/// crate's own README recommended the import line that then broke the next
/// `Send + Sync` bound (`docs/DEFECT_PATTERNS.md` 2.4). The concept is
/// cadence, not synchronisation.
///
/// Where the confirmed line sits is not one of these numbers. The chain states
/// it itself (`docs/design/030 §8 (ex-025)`): Ethereum and Solana ask their node, so a
/// number here would be a lie, and Bitcoin, which has no finality, keeps a
/// confirmation depth of its own in its own section. `reorg_retention`
/// described a rollback journal that no longer exists.
///
/// Both fields are the type their consumer takes. [`Self::poll`] is the
/// `Duration` `wallets::ComposedChain::poll` holds, and [`Self::policy`] is
/// the [`SyncPolicy`] `indexing` validates — neither is re-inflated from a
/// bare integer at the chain end, which is what this type used to force
/// because a `packages/*` crate could not name an `sdk/*` type.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(from = "Keys")]
pub struct Cadence {
    /// How long this chain waits between two tip observations.
    pub poll: Duration,
    /// How much one pass may read, and how far below the checkpoint a new
    /// watch may start.
    pub policy: SyncPolicy,
}

impl Cadence {
    /// The rhythm every chain starts from: a one-second cadence and a
    /// hundred-block pass.
    ///
    /// `serde` fills an absent key by calling the same functions this does, so
    /// a value built in code and a value read from a document cannot drift.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            poll: Self::default_poll(),
            policy: SyncPolicy::new(Self::default_batch_size()),
        }
    }

    /// Rejects a rhythm a synchronizer cannot run on.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Cadence`] when the poll interval is zero, or when
    /// `indexing` refuses the policy.
    pub fn validate(&self) -> Result<(), Error> {
        if self.poll.is_zero() || self.policy.validate().is_err() {
            return Err(Error::new(
                ErrorKind::Cadence,
                "index polling and batch values must be positive",
            ));
        }
        Ok(())
    }

    const fn default_poll() -> Duration {
        Duration::from_millis(1_000)
    }

    const fn default_batch_size() -> usize {
        100
    }

    const fn default_max_backfill_blocks() -> u64 {
        SyncPolicy::new(Self::default_batch_size()).max_backfill_blocks
    }
}

impl Default for Cadence {
    fn default() -> Self {
        Self::new()
    }
}

/// What a document writes, in the units an operator writes them in.
///
/// It exists so [`Cadence`] can hold a [`Duration`] and a [`SyncPolicy`]
/// while a file still says `poll_millis` and `batch_size`. Every default here
/// calls the same function [`Cadence::new`] calls, so the two cannot drift,
/// and the schema is closed so a retired key fails loudly.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    #[serde(
        default = "Cadence::default_poll",
        deserialize_with = "crate::millis::deserialize"
    )]
    poll_millis: Duration,
    #[serde(default = "Cadence::default_batch_size")]
    batch_size: usize,
    #[serde(default = "Cadence::default_max_backfill_blocks")]
    max_backfill_blocks: u64,
}

impl From<Keys> for Cadence {
    fn from(keys: Keys) -> Self {
        Self {
            poll: keys.poll_millis,
            policy: SyncPolicy::new(keys.batch_size).with_backfill(keys.max_backfill_blocks),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn parse(value: Value) -> Result<Cadence, Error> {
        serde_json::from_value(value)
            .map_err(|error| Error::new(ErrorKind::Cadence, error.to_string()))
    }

    /// `serde` fills an absent key by calling the same function `new` calls,
    /// so an empty document and the constructor cannot disagree.
    #[test]
    fn an_absent_bound_is_the_constructors_own() {
        assert_eq!(
            parse(json!({})).expect("every key defaults"),
            Cadence::new()
        );
        assert_eq!(Cadence::new(), Cadence::default());
        assert_eq!(
            parse(json!({"batch_size": 250})).expect("one key stated"),
            Cadence {
                policy: SyncPolicy::new(250),
                ..Cadence::new()
            }
        );
        let explicit = parse(json!({"poll_millis": 17, "batch_size": 7})).expect("both stated");
        assert_eq!(explicit.poll, Duration::from_millis(17));
        assert_eq!(explicit.policy.batch_size, 7);
        assert_eq!(
            explicit.policy.max_backfill_blocks,
            Cadence::new().policy.max_backfill_blocks,
            "the backfill bound is indexing's own unless the document states one"
        );
        assert_eq!(
            parse(json!({"max_backfill_blocks": 5}))
                .expect("the third key")
                .policy
                .max_backfill_blocks,
            5
        );
    }

    #[test]
    fn a_zero_bound_is_rejected_from_code_and_from_a_document_alike() {
        for (coded, written) in [
            (
                Cadence {
                    poll: Duration::ZERO,
                    ..Cadence::new()
                },
                json!({"poll_millis": 0}),
            ),
            (
                Cadence {
                    policy: SyncPolicy::new(0),
                    ..Cadence::new()
                },
                json!({"batch_size": 0}),
            ),
        ] {
            let built = coded.validate().expect_err("a zero bound in code");
            let read = parse(written)
                .expect("a zero parses")
                .validate()
                .expect_err("and is rejected by the same check");
            assert_eq!(
                built.to_string(),
                "index polling and batch values must be positive"
            );
            assert_eq!((built.kind, built.message), (read.kind, read.message));
        }
        Cadence::new().validate().expect("the defaults are usable");
    }

    #[test]
    fn the_cadence_wire_contract_is_closed() {
        for raw in [
            r#"{"poll_millis":1,"batch_size":2,"confirmation_depth":6}"#,
            r#"{"reorg_retention":100}"#,
            r#"{"poll_millis":1,"poll_millis":2}"#,
            r#"{"poll_millis":null}"#,
            r#"{"batch_size":"7"}"#,
            r#"{"poll":1000}"#,
        ] {
            serde_json::from_str::<Value>(raw).expect("syntactically valid raw JSON");
            assert!(
                serde_json::from_str::<Cadence>(raw).is_err(),
                "{raw} must be rejected"
            );
        }
    }
}
