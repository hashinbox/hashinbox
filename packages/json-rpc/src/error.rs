use std::{error::Error as StdError, fmt};

use crate::Failure;

/// Classification of a transport [`Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// An [`crate::Endpoint`] — its URL, a header, a bound or a `${NAME}`
    /// reference — or an endpoint list is invalid.
    InvalidConfiguration,
    /// The request was rejected locally before sending (params, empty batch).
    InvalidRequest,
    /// The HTTP execution timed out (retryable).
    Timeout,
    /// No endpoint could be reached (retryable).
    Unavailable,
    /// The endpoint answered with a non-success HTTP status (429/502/503/504 retryable).
    HttpStatus(u16),
    /// A body exceeded the endpoint's configured [`crate::Bounds`].
    ResponseTooLarge,
    /// The response is not valid JSON-RPC or does not decode into the requested type.
    InvalidResponse,
}

/// A transport failure: the call produced no server answer to interpret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What went wrong, for callers to branch on.
    pub kind: ErrorKind,
    /// Human-readable context; never contains endpoint URLs or header values.
    pub message: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Whether repeating the request may succeed: timeouts, unavailable
    /// endpoints, and HTTP 429/502/503/504.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Timeout
                | ErrorKind::Unavailable
                | ErrorKind::HttpStatus(429 | 502 | 503 | 504)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

/// Why one JSON-RPC call returned no result.
///
/// The transport either failed locally ([`Error`]) or the server answered with
/// a JSON-RPC error object ([`Failure`]). Both convert with `From`, so a call
/// flattens with `client.request(method, params).await??`. Chains keep their
/// own retry policy for remote codes and map this into their native error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The transport failed locally.
    Transport(Error),
    /// The server returned a JSON-RPC error object.
    Remote(Failure),
}

impl CallError {
    /// The server's JSON-RPC error code; `None` for a local transport failure.
    #[must_use]
    pub const fn remote_code(&self) -> Option<i64> {
        match self {
            Self::Transport(_) => None,
            Self::Remote(failure) => Some(failure.code),
        }
    }

    /// Default policy: retryable transport failures and implementation-defined
    /// server errors (`-32099..=-32000`) may be retried.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(error) => error.is_retryable(),
            Self::Remote(failure) => failure.is_server_error(),
        }
    }
}

impl From<Error> for CallError {
    fn from(error: Error) -> Self {
        Self::Transport(error)
    }
}

impl From<Failure> for CallError {
    fn from(failure: Failure) -> Self {
        Self::Remote(failure)
    }
}

/// Remote messages and data are server-controlled, so only the code is shown.
impl fmt::Display for CallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => fmt::Display::fmt(error, formatter),
            Self::Remote(failure) => {
                write!(formatter, "JSON-RPC call failed with code {}", failure.code)
            }
        }
    }
}

impl StdError for CallError {}
