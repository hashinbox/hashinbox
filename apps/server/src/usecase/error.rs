//! [`Error`]: why a usecase failed, by [`Kind`], with what to tell the
//! caller. A transport answers by the kind (the HTTP API: one status each).

use crate::models::IdError;

/// What kind of failure an [`Error`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The input breaks a rule: malformed, or refused as it is.
    Invalid,
    /// What the input names is not kept, or not configured here.
    NotFound,
    /// Well formed, but what is kept refuses it: an id taken, a wallet
    /// without its key.
    Conflict,
    /// A store, a node, the queue or key generation failed.
    Unavailable,
    /// This server failed: its clock.
    Internal,
}

/// A usecase failure: its kind, its message, and the transfer of a send it
/// is about.
#[derive(Debug)]
pub(crate) struct Error {
    pub(crate) kind: Kind,
    /// What went wrong.
    pub(crate) message: String,
    /// Zero-based index of the transfer of a send that failed.
    pub(crate) failed_index: Option<usize>,
}

impl Error {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(Kind::Invalid, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(Kind::Conflict, message)
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Kind::Unavailable, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(Kind::Internal, message)
    }

    /// The same failure, attributed to transfer `failed_index` of a send.
    #[must_use]
    pub(crate) fn at(self, failed_index: usize) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..self
        }
    }

    fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            failed_index: None,
        }
    }
}

/// A malformed id: invalid.
impl From<IdError> for Error {
    fn from(error: IdError) -> Self {
        Self::invalid(error.to_string())
    }
}

/// The handled failures inside an `anyhow::Error` by kind: an unconfigured
/// network is not found, a bad address or key invalid; any other failure (a
/// store, a node, generation) is unavailable.
impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        let kind = match error.downcast_ref::<base::WalletError>().map(|e| e.kind) {
            Some(base::WalletErrorKind::Unsupported) => Kind::NotFound,
            Some(base::WalletErrorKind::InvalidAddress | base::WalletErrorKind::InvalidSecret) => {
                Kind::Invalid
            }
            _ => Kind::Unavailable,
        };
        Self::new(kind, format!("{error:#}"))
    }
}
