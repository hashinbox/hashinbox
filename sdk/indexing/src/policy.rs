//! The two bounds a pass works inside: how much it may read, and how far
//! below the checkpoint a new watch may start.

use crate::{IndexError, IndexErrorKind};

/// How far below the checkpoint a new watch may start, by default.
///
/// Deliberately a number an operator can exceed on purpose and cannot reach by
/// typo. A mistyped start is the runaway this bounds: it is refused at
/// [`crate::Watching::watch`], synchronously, before any block is fetched,
/// because a background failure an hour later is not a refusal.
const MAX_BACKFILL_BLOCKS: u64 = 100_000;

/// Chain-neutral synchronization policy shared by concrete indexers.
///
/// Two numbers, both of them bounds on work: how much one pass may read, and
/// how much history a single new watch may ask for. Rollback retention and the
/// confirmation line are gone because the chain now answers them itself — the
/// line is [`ChainTips::confirmed_tip`], stated by the source rather than
/// guessed at in a configuration file, and retention described a journal that
/// no longer exists, because nothing the store holds is ever undone
/// (`docs/design/030 §8 (ex-025)`).
///
/// [`ChainTips::confirmed_tip`]: crate::ChainTips::confirmed_tip
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncPolicy {
    /// Maximum blocks read in one `sync` pass, so a pass stays bounded.
    ///
    /// The forward phase spends it first and the backfill phase gets what is
    /// left, which is what keeps a backfill from starving live indexing: a
    /// network that is behind its tip does no backfill at all until it catches
    /// up.
    pub batch_size: usize,
    /// Maximum blocks below the checkpoint a new watch may start at.
    ///
    /// Zero refuses every historical start, which is the pre-backfill
    /// behaviour stated as a policy instead of happening silently. It bounds
    /// what a *caller* may newly ask for; it does not bound the one-time
    /// coverage sweep an upgraded store performs for addresses registered
    /// before backfill existed.
    pub max_backfill_blocks: u64,
}

impl SyncPolicy {
    /// A policy from its batch size, with the default backfill bound.
    #[must_use]
    pub const fn new(batch_size: usize) -> Self {
        Self {
            batch_size,
            max_backfill_blocks: MAX_BACKFILL_BLOCKS,
        }
    }

    /// The same policy, bounding a new watch's history to `blocks`.
    #[must_use]
    pub const fn with_backfill(self, blocks: u64) -> Self {
        Self {
            max_backfill_blocks: blocks,
            ..self
        }
    }

    /// Rejects a start further below `checkpoint` than the policy allows.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`], naming the earliest start this
    /// policy would accept, so the caller can retry with a legal one instead
    /// of guessing.
    pub(crate) fn admits(
        &self,
        start: crate::BlockPosition,
        checkpoint: crate::BlockPosition,
    ) -> Result<(), IndexError> {
        let earliest = checkpoint.0.saturating_sub(self.max_backfill_blocks);
        if start.0 >= earliest {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::InvalidRequest,
            format!(
                "a watch may start at most {} blocks below the checkpoint at {}; \
                 the earliest acceptable start is {earliest}",
                self.max_backfill_blocks, checkpoint.0
            ),
            false,
        ))
    }

    /// Checks that the batch size is greater than zero.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] for a zero batch size.
    pub fn validate(&self) -> Result<(), IndexError> {
        if self.batch_size == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "batch size must be greater than zero",
                false,
            ));
        }
        Ok(())
    }
}

/// A network and its validated [`SyncPolicy`]: everything a [`crate::Indexer`]
/// needs besides its source, interpreter, and repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncConfig {
    pub(crate) network: crate::Network,
    pub(crate) policy: SyncPolicy,
}

impl SyncConfig {
    /// Binds `policy` to `network`.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] when [`SyncPolicy::validate`] fails.
    pub fn new(network: &impl crate::Networked, policy: SyncPolicy) -> Result<Self, IndexError> {
        policy.validate()?;
        Ok(Self {
            network: network.canonical(),
            policy,
        })
    }

    /// The validated policy this configuration binds.
    #[must_use]
    pub const fn policy(&self) -> SyncPolicy {
        self.policy
    }
}
