use std::{error::Error as StdError, fmt};

use crate::Failure;

/// Classification of a transport [`Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// An [`crate::Endpoint`] — its URL or a bound — or an endpoint list is
    /// invalid.
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
    /// The node answered a rate or size limit error object with this code
    /// ([`Failure::is_rate_limited`]; retryable).
    RateLimited(i64),
}

/// A transport failure: the call produced no server answer to interpret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What went wrong, for callers to branch on.
    pub kind: ErrorKind,
    /// Human-readable context; never contains endpoint URLs.
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
    /// endpoints, HTTP 429/502/503/504, and rate-limited answers.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Timeout
                | ErrorKind::Unavailable
                | ErrorKind::HttpStatus(429 | 502 | 503 | 504)
                | ErrorKind::RateLimited(_)
        )
    }

    /// `failure` as a retryable transport error when it is rate-limited, so
    /// [`crate::Retry`] backs off and [`crate::Failover`] moves on.
    pub(crate) fn limited(failure: Failure) -> Result<Failure, Self> {
        if !failure.is_rate_limited() {
            return Ok(failure);
        }
        Err(Self::new(
            ErrorKind::RateLimited(failure.code),
            failure.to_string(),
        ))
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

/// A remote failure shows its code and the node's message, truncated; never
/// its data.
impl fmt::Display for CallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => fmt::Display::fmt(error, formatter),
            Self::Remote(failure) => fmt::Display::fmt(failure, formatter),
        }
    }
}

impl StdError for CallError {}

#[cfg(test)]
mod tests {
    use super::{CallError, Error, ErrorKind};
    use crate::{Failure, Statistics};

    fn failure(code: i64, message: &str) -> Failure {
        Failure {
            code,
            message: message.to_owned(),
            data: None,
        }
    }

    #[test]
    fn rate_limits_become_retryable_transport_errors() {
        for (code, message) in [
            (-32_005, "Rate limit exceeded"),
            (-32_005, "query returned more than 10000 results"),
            (-32_003, "daily request limit reached"),
        ] {
            let error = Error::limited(failure(code, message)).unwrap_err();
            assert_eq!(error.kind, ErrorKind::RateLimited(code));
            assert!(error.is_retryable(), "{code} {message}");
            assert!(error.message.ends_with(message), "{}", error.message);
        }
        for (code, message) in [
            (-32_003, "transaction rejected"),
            (-32_000, "execution reverted"),
            (-32_601, "the method does not exist"),
        ] {
            assert_eq!(
                Error::limited(failure(code, message)),
                Ok(failure(code, message))
            );
        }
        assert!(Error::new(ErrorKind::HttpStatus(429), "slow down").is_retryable());
    }

    #[test]
    fn a_remote_failure_shows_the_nodes_message_truncated() {
        let shown = CallError::Remote(failure(-32_005, "Rate limit exceeded")).to_string();
        assert_eq!(
            shown,
            "JSON-RPC call failed with code -32005: Rate limit exceeded"
        );
        let long = CallError::Remote(failure(-32_000, &"x".repeat(500))).to_string();
        assert!(long.ends_with(&format!("{}…", "x".repeat(200))), "{long}");
    }

    #[test]
    fn a_rate_limit_counts_as_the_nodes_code() {
        let statistics = Statistics::default();
        let error = Error::limited(failure(-32_005, "Rate limit exceeded")).unwrap_err();
        let text = statistics.call(&Err(error));
        assert_eq!(
            text.as_deref(),
            Some("JSON-RPC call failed with code -32005: Rate limit exceeded")
        );
        assert!(statistics.limited(&[crate::Limit::Remote(-32_005)]));
    }
}
