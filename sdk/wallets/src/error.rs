use std::{error, fmt};

/// The wallet failures a caller handles differently, carried inside an
/// [`anyhow::Error`] and found with `downcast_ref::<Error>()`. Every
/// other failure (a store, a bug, key generation) is a plain `anyhow::Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// No factory for the network or asset a request named.
    Unsupported,
    /// Imported secret material is not a valid key.
    InvalidSecret,
    /// Address text or network is wrong.
    InvalidAddress,
    /// A node or store is unavailable; retrying may succeed.
    Unavailable,
}

impl ErrorKind {
    /// Whether retrying the same request may succeed.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// A handled wallet failure: its kind and a message without key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// Stable classification.
    pub kind: ErrorKind,
    /// Human-readable detail; never contains key material.
    pub message: String,
}

impl Error {
    /// Builds an error of `kind`.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}
