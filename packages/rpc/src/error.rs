//! Why a call returned no result, and the one retry and rate-limit policy.

use std::{error::Error as StdError, fmt};

use crate::RawJson;

/// Why a call returned no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The node answered with a JSON-RPC error object.
    Remote(Failure),
    /// No answer to interpret: configuration, a local request check, a
    /// timeout, an unreachable node, an HTTP status, an oversized or invalid
    /// body.
    Transport {
        /// What went wrong, for callers to branch on.
        kind: ErrorKind,
        /// Human-readable context; never contains endpoint URLs.
        message: String,
    },
}

/// Classification of a [`Error::Transport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// An [`crate::Endpoint`] — its URL or a setting — or a composition (a
    /// route) is invalid.
    InvalidConfiguration,
    /// The request was rejected locally before sending (params, empty batch).
    InvalidRequest,
    /// The HTTP execution timed out (retryable).
    Timeout,
    /// No endpoint could be reached (retryable).
    Unavailable,
    /// The endpoint answered with a non-success HTTP status (429/502/503/504 retryable).
    HttpStatus(u16),
    /// A response exceeded the endpoint's `rpc.max_response_bytes`.
    ResponseTooLarge,
    /// The response is not valid JSON-RPC or does not decode into the requested type.
    InvalidResponse,
}

/// A JSON-RPC error object returned by the server for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// JSON-RPC error code.
    pub code: i64,
    /// Server-controlled message; operators see it (truncated) in
    /// [`fmt::Display`], end users should not.
    pub message: String,
    /// Optional server-controlled detail.
    pub data: Option<RawJson>,
}

impl Error {
    pub(crate) fn transport(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::Transport {
            kind,
            message: message.into(),
        }
    }

    /// The transport kind; `None` for an error object.
    #[must_use]
    pub const fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Transport { kind, .. } => Some(*kind),
            Self::Remote(_) => None,
        }
    }

    /// Whether repeating the request may succeed: a timeout, an unreachable
    /// node, HTTP 429/502/503/504, or a rate limit ([`Self::is_rate_limited`]).
    /// Any other error object is the node's answer, so repeating it repeats
    /// the answer.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { kind, .. } => matches!(
                kind,
                ErrorKind::Timeout
                    | ErrorKind::Unavailable
                    | ErrorKind::HttpStatus(429 | 502 | 503 | 504)
            ),
            Self::Remote(failure) => failure.is_rate_limited(),
        }
    }

    /// Whether the node refused for a rate limit: HTTP 429, or an error
    /// object [`Failure::is_rate_limited`] says is one.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        match self {
            Self::Transport { kind, .. } => *kind == ErrorKind::HttpStatus(429),
            Self::Remote(failure) => failure.is_rate_limited(),
        }
    }

    /// Whether the node is not at what was asked yet: an error object
    /// [`Failure::is_not_ready`] says is one. No failure: the node answered.
    #[must_use]
    pub fn is_not_ready(&self) -> bool {
        matches!(self, Self::Remote(failure) if failure.is_not_ready())
    }

    /// Whether the work was refused before sending, so no node was asked.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(
            self,
            Self::Transport {
                kind: ErrorKind::InvalidRequest,
                ..
            }
        )
    }

    /// A short name for logs: `timeout`, `connect`, `http_429`,
    /// `too_large`, `invalid_response`, `invalid_request`,
    /// `invalid_configuration`, or `rpc_{code}` for an error
    /// object.
    #[must_use]
    pub fn label(&self) -> String {
        let kind = match self {
            Self::Remote(failure) => return format!("rpc_{}", failure.code),
            Self::Transport { kind, .. } => kind,
        };
        let name = match kind {
            ErrorKind::HttpStatus(status) => return format!("http_{status}"),
            ErrorKind::InvalidConfiguration => "invalid_configuration",
            ErrorKind::InvalidRequest => "invalid_request",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Unavailable => "connect",
            ErrorKind::ResponseTooLarge => "too_large",
            ErrorKind::InvalidResponse => "invalid_response",
        };
        name.to_owned()
    }

    /// This error as one nonzero number, for lock-free counters: the kind's
    /// [`ErrorKind::code`], or [`REMOTE`] plus the error object's code.
    pub(crate) fn code(&self) -> u64 {
        match self {
            Self::Transport { kind, .. } => kind.code(),
            Self::Remote(failure) => REMOTE | u64::from(failure.code as u32),
        }
    }

    /// What a [`Self::code`] says, with no message from the node; `None` for
    /// zero or a code no error has.
    pub(crate) fn describe(code: u64) -> Option<String> {
        if code & REMOTE != 0 {
            let code = code as u32 as i32;
            return Some(format!("JSON-RPC call failed with code {code}"));
        }
        ErrorKind::from_code(code).map(|kind| kind.to_string())
    }
}

/// The bit of an [`Error::code`] that marks an error object's code.
const REMOTE: u64 = 1 << 32;

/// The bit of an [`ErrorKind::code`] that marks an HTTP status, in its low 16 bits.
const HTTP: u64 = 1 << 16;

/// Each kind's code but [`ErrorKind::HttpStatus`], which is `HTTP | status`.
const CODES: [(ErrorKind, u64); 6] = [
    (ErrorKind::InvalidConfiguration, 1),
    (ErrorKind::InvalidRequest, 2),
    (ErrorKind::Timeout, 3),
    (ErrorKind::Unavailable, 4),
    (ErrorKind::ResponseTooLarge, 5),
    (ErrorKind::InvalidResponse, 6),
];

impl ErrorKind {
    /// This kind as a number, by [`CODES`].
    fn code(self) -> u64 {
        if let Self::HttpStatus(status) = self {
            return HTTP | u64::from(status);
        }
        CODES
            .iter()
            .find(|(kind, _)| *kind == self)
            .map_or(0, |(_, code)| *code)
    }

    /// The kind a [`Self::code`] was made from.
    fn from_code(code: u64) -> Option<Self> {
        if code & !0xFFFF == HTTP {
            return Some(Self::HttpStatus(code as u16));
        }
        CODES
            .iter()
            .find(|(_, known)| *known == code)
            .map(|(kind, _)| *kind)
    }
}

impl From<Failure> for Error {
    fn from(failure: Failure) -> Self {
        Self::Remote(failure)
    }
}

/// A transport error shows its message; an error object its code and the
/// node's message, truncated, never its data.
impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { message, .. } => formatter.write_str(message),
            Self::Remote(failure) => fmt::Display::fmt(failure, formatter),
        }
    }
}

impl StdError for Error {}

/// The classification, without detail.
impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::InvalidConfiguration => "JSON-RPC endpoint configuration is invalid",
            Self::InvalidRequest => "JSON-RPC request is invalid",
            Self::Timeout => "JSON-RPC request timed out",
            Self::Unavailable => "JSON-RPC transport is unavailable",
            Self::HttpStatus(status) => {
                return write!(formatter, "JSON-RPC endpoint returned HTTP {status}");
            }
            Self::ResponseTooLarge => "JSON-RPC response exceeded its configured limit",
            Self::InvalidResponse => "JSON-RPC endpoint returned an invalid response",
        };
        formatter.write_str(text)
    }
}

/// The most characters of a node's message a [`Failure`] shows.
const SHOWN: usize = 200;

impl Failure {
    /// The node's message cut to its first 200 characters, `…` when cut.
    #[must_use]
    pub fn shown(&self) -> String {
        let mut shown: String = self.message.chars().take(SHOWN).collect();
        if shown.len() < self.message.len() {
            shown.push('…');
        }
        shown
    }

    /// Whether the code is in the implementation-defined server range `-32099..=-32000`.
    #[must_use]
    pub const fn is_server_error(&self) -> bool {
        self.code >= -32_099 && self.code <= -32_000
    }

    /// Whether the node refused for a rate or size limit, so the same call
    /// may succeed later: `-32005` ("limit exceeded"), or `-32003` when its
    /// message says "rate limit" or "request limit".
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        let message = self.message.to_ascii_lowercase();
        match self.code {
            -32_005 => true,
            -32_003 => message.contains("rate limit") || message.contains("request limit"),
            _ => false,
        }
    }

    /// Whether the node has not reached the block asked for yet: `-32602`
    /// saying the range "extends beyond current head" (reth, Erigon). It
    /// answered; the block comes later.
    #[must_use]
    pub fn is_not_ready(&self) -> bool {
        self.code == -32_602 && self.message.contains("beyond current head")
    }

    /// Whether the node refused because it does not serve the method, so
    /// another node may: `-32601` (method not found), or Tatum's `-16401`
    /// (a method of paid plans only).
    #[must_use]
    pub const fn is_unserved(&self) -> bool {
        matches!(self.code, -32_601 | -16_401)
    }
}

/// The code and the node's message, cut ([`Failure::shown`]).
impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "JSON-RPC call failed with code {}: {}",
            self.code,
            self.shown()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind, Failure};

    fn failure(code: i64, message: &str) -> Error {
        Error::Remote(Failure {
            code,
            message: message.to_owned(),
            data: None,
        })
    }

    #[test]
    fn a_rate_limit_stays_remote_and_is_retryable() {
        for (code, message) in [
            (-32_005, "Rate limit exceeded"),
            (-32_005, "query returned more than 10000 results"),
            (-32_003, "daily request limit reached"),
        ] {
            let error = failure(code, message);
            assert!(error.is_rate_limited() && error.is_retryable(), "{message}");
            assert_eq!(error.kind(), None);
        }
        for (code, message) in [
            (-32_003, "transaction rejected"),
            (-32_003, "exceeds block gas limit"),
            (-32_000, "execution reverted"),
            (-32_601, "the method does not exist"),
        ] {
            let error = failure(code, message);
            assert!(
                !error.is_rate_limited() && !error.is_retryable(),
                "{message}"
            );
        }
        let slow = Error::transport(ErrorKind::HttpStatus(429), "slow down");
        assert!(slow.is_retryable() && slow.is_rate_limited());
        let busy = Error::transport(ErrorKind::HttpStatus(503), "busy");
        assert!(busy.is_retryable() && !busy.is_rate_limited());
    }

    #[test]
    fn a_method_the_node_does_not_serve_is_unserved() {
        let unserved = |code, message| match failure(code, message) {
            Error::Remote(failure) => failure.is_unserved(),
            Error::Transport { .. } => unreachable!(),
        };
        assert!(unserved(-32_601, "Method not found"));
        assert!(unserved(
            -16_401,
            "Method 'getTokenAccountsByOwner' is available for paid plans only."
        ));
        assert!(!unserved(-32_000, "execution reverted"));
        assert!(!unserved(
            -32_016,
            "Minimum context slot has not been reached"
        ));
    }

    #[test]
    fn every_kind_and_remote_code_round_trips() {
        for kind in [
            ErrorKind::InvalidConfiguration,
            ErrorKind::InvalidRequest,
            ErrorKind::Timeout,
            ErrorKind::Unavailable,
            ErrorKind::HttpStatus(429),
            ErrorKind::HttpStatus(u16::MAX),
            ErrorKind::ResponseTooLarge,
            ErrorKind::InvalidResponse,
        ] {
            let code = Error::transport(kind, "detail").code();
            assert_ne!(code, 0);
            assert_eq!(ErrorKind::from_code(code), Some(kind));
            assert_eq!(Error::describe(code), Some(kind.to_string()));
        }
        for code in [-32_000, -32_601, 0, 1, i64::from(i32::MIN)] {
            let shown = Error::describe(failure(code, "secret").code());
            assert_eq!(
                shown,
                Some(format!("JSON-RPC call failed with code {code}"))
            );
        }
        assert_eq!(Error::describe(0), None);
    }

    #[test]
    fn a_remote_failure_shows_the_nodes_message_truncated() {
        let shown = failure(-32_005, "Rate limit exceeded").to_string();
        assert_eq!(
            shown,
            "JSON-RPC call failed with code -32005: Rate limit exceeded"
        );
        let long = failure(-32_000, &"x".repeat(500)).to_string();
        assert!(long.ends_with(&format!("{}…", "x".repeat(200))), "{long}");
    }
}
