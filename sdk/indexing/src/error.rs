//! **Contract.**
//!
//! The two indexing failures and their classification: `SourceError` (a
//! `BlockSource` could not answer) and `IndexError` / `IndexErrorKind` (every
//! other indexing failure). `retryable` is what the driver acts on, and
//! `IndexErrorKind` is what a caller maps to its own vocabulary.

use std::{error::Error, fmt};

/// A [`BlockSource`](crate::BlockSource) failure. Converts into an
/// [`IndexError`] of kind [`IndexErrorKind::Source`], keeping `retryable`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceError {
    /// Human-readable reason; never contains credentials.
    pub message: String,
    /// Whether trying the same call again can succeed (a busy node, a timeout).
    pub retryable: bool,
}

/// Every indexing failure: a category, a reason, and whether to retry.
///
/// `retryable` is what the [`Driver`](crate::Driver) acts on: a
/// retryable error is retried forever with growing backoff, and anything else
/// quarantines that one network while the others keep syncing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexError {
    /// What went wrong, for callers that map errors (for example to HTTP).
    pub kind: IndexErrorKind,
    /// Human-readable reason.
    pub message: String,
    /// Whether trying the same operation again can succeed.
    pub retryable: bool,
}

/// Category of an [`IndexError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexErrorKind {
    /// The chain source (RPC) failed.
    Source,
    /// The repository failed or returned inconsistent state.
    Store,
    /// A concurrent change won: another pass is running, the checkpoint moved
    /// under a cursor, or a registration already exists.
    Conflict,
    /// A request, address, or block belongs to another `(chain, network)` pair.
    NetworkMismatch,
    /// Block content is wrong in itself, whatever the chain is doing: native
    /// content violates its protocol or the canonical invariants, or a
    /// source's answer contradicts what was asked of it (more blocks than the
    /// limit, a block outside the range, positions that do not ascend, a
    /// parent on genesis or none above it).
    ///
    /// The kind does not decide the retry, [`IndexError::retryable`] does:
    /// content this crate already holds is terminal, because reading it again
    /// reads the same bytes, while an answer a source gave is asked for again
    /// on the next pass.
    InvalidBlock,
    /// Well-formed blocks that do not join up: with the checkpoint, with the
    /// block before them, or with what the source itself reported a moment
    /// earlier. Usually a fork in flight; the next pass reconciles.
    Disconnected,
    /// A block the store already committed is no longer canonical: the chain
    /// reorganized past its own confirmed line. Impossible on a chain with
    /// finality; on Bitcoin it means a reorg deeper than the configured
    /// confirmation depth. The network needs an offline rescan.
    ConfirmedReorg,
    /// The caller passed invalid input (zero limits, duplicate filters, ...).
    InvalidRequest,
}

impl SourceError {
    /// A source failure with its retry decision.
    #[must_use]
    pub fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
        }
    }
}

impl IndexError {
    /// An error of `kind` with its retry decision.
    #[must_use]
    pub fn new(kind: IndexErrorKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }

    /// A block whose native content violates its protocol; retrying cannot fix it.
    #[must_use]
    pub fn invalid_block(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::InvalidBlock, message, false)
    }

    /// A source's answer that contradicts the request or itself. Nothing of
    /// it was held or committed and the next pass asks the source again, so
    /// unlike [`Self::invalid_block`] it is retried.
    pub(crate) fn invalid_answer(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::InvalidBlock, message, true)
    }

    /// Blocks that do not join up while the chain is moving; the next pass
    /// reads it again.
    pub(crate) fn disconnected(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::Disconnected, message, true)
    }

    pub(crate) fn retryable_conflict(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::Conflict, message, true)
    }

    /// The confirmed checkpoint itself reorged; only a rescan can repair it.
    pub(crate) fn confirmed_reorg(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::ConfirmedReorg, message, false)
    }
}

impl From<SourceError> for IndexError {
    fn from(error: SourceError) -> Self {
        Self::new(IndexErrorKind::Source, error.message, error.retryable)
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for SourceError {}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for IndexError {}
