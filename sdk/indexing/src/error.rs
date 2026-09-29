//! The two indexing failures and their classification: `SourceError` (a
//! `BlockSource` could not answer) and `IndexError` / `IndexErrorKind` (every
//! other indexing failure). `retryable` is what the supervisor acts on, and
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
/// `retryable` is what the [`Supervisor`](crate::Supervisor) acts on: a
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
    /// Native block content violates its protocol or the canonical invariants.
    InvalidBlock,
    /// The source's blocks do not link to the checkpoint (usually a reorg in
    /// flight); the next pass reconciles.
    CannotConnect,
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

    pub(crate) fn cannot_connect(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::CannotConnect, message, true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_error_keeps_message_and_retryability() {
        let error = SourceError::new("node busy", true);

        assert_eq!(error.message, "node busy");
        assert!(error.retryable);
        assert_eq!(IndexError::from(error).kind, IndexErrorKind::Source);
    }

    #[test]
    fn invalid_block_is_terminal() {
        let error = IndexError::invalid_block("duplicate transaction id");

        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(error.message, "duplicate transaction id");
        assert!(!error.retryable);
    }
}
