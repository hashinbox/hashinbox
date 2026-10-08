use std::time::Duration;

use indexing::Budget;
use serde::Deserialize;

use crate::config::{Error, ErrorKind};

/// The rhythm one chain indexes at: how often it asks its node for a new tip,
/// and the bounds one synchronization pass runs under.
///
/// This used to be called `Sync`, which shadowed [`std::marker::Sync`] — the
/// crate's own README recommended the import line that then broke the next
/// `Send + Sync` bound (`docs/DEFECT_PATTERNS.md` 2.4). The concept is
/// cadence, not synchronisation.
///
/// Where the confirmed line sits is not one of these numbers. The chain states
/// it itself : Ethereum and Solana ask their node, so a
/// number here would be a lie, and Bitcoin, which has no finality, keeps a
/// confirmation depth of its own in its own section. `reorg_retention`
/// described a rollback journal that no longer exists.
///
/// Both fields are the type their consumer takes. [`Self::poll`] is the
/// `Duration` a poll schedule (`indexing::Schedule`) holds, and [`Self::policy`] is
/// the [`Budget`] `indexing` validates — neither is re-inflated from a
/// bare integer at the chain end, which is what this type used to force
/// because a `packages/*` crate could not name an `sdk/*` type.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(from = "Keys")]
pub struct Cadence {
    /// The operator's override of how long this chain waits between two tip
    /// observations. `None`: the rhythm the chain itself declares.
    pub poll: Option<Duration>,
    /// How much one pass may read, and how far below the checkpoint a new
    /// watch may start.
    pub policy: Budget,
}

impl Cadence {
    /// The rhythm every chain starts from: the chain's own poll cadence and a
    /// hundred-block pass.
    ///
    /// `serde` fills an absent key by calling the same functions this does, so
    /// a value built in code and a value read from a document cannot drift.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            poll: None,
            policy: Budget::new(Self::default_batch_size()),
        }
    }

    /// Rejects a rhythm a synchronizer cannot run on.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Cadence`] when the poll interval is zero, or when
    /// `indexing` refuses the policy.
    pub fn validate(&self) -> Result<(), Error> {
        if self.poll.is_some_and(|poll| poll.is_zero()) || self.policy.validate().is_err() {
            return Err(Error::new(
                ErrorKind::Cadence,
                "index polling and batch values must be positive",
            ));
        }
        Ok(())
    }

    const fn default_batch_size() -> usize {
        100
    }

    const fn default_max_backfill_blocks() -> u64 {
        Budget::new(Self::default_batch_size()).max_backfill_blocks
    }
}

impl Default for Cadence {
    fn default() -> Self {
        Self::new()
    }
}

/// What a document writes, in the units an operator writes them in.
///
/// It exists so [`Cadence`] can hold a [`Duration`] and a [`Budget`]
/// while a file still says `poll_millis` and `batch_size`. Every default here
/// calls the same function [`Cadence::new`] calls, so the two cannot drift,
/// and the schema is closed so a retired key fails loudly.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    #[serde(default, deserialize_with = "override_millis")]
    poll_millis: Option<Duration>,
    #[serde(default = "Cadence::default_batch_size")]
    batch_size: usize,
    #[serde(default = "Cadence::default_max_backfill_blocks")]
    max_backfill_blocks: u64,
}

/// A stated `poll_millis` overrides the chain's rhythm; `null` is refused, not
/// read as absent.
fn override_millis<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    crate::config::millis::deserialize(deserializer).map(Some)
}

impl From<Keys> for Cadence {
    fn from(keys: Keys) -> Self {
        Self {
            poll: keys.poll_millis,
            policy: Budget::new(keys.batch_size).with_backfill(keys.max_backfill_blocks),
        }
    }
}
