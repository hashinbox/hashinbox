//! How JSON-RPC failures become [`Error`]s, and how remote failures are read.
//!
//! Two retry policies exist on purpose. Wallet reads and preflight retry the
//! codes and messages Ethereum nodes use for overload. Block reads for
//! indexing use the JSON-RPC default: retry only implementation-defined
//! server errors. Neither exposes node text, which may contain secrets.

use json_rpc::CallError;

use crate::native::Error;

/// RPC service codes that mean "overloaded or internal, try again".
const RETRYABLE_CODES: [i64; 3] = [-32_605, -32_603, -32_005];
/// Lower-case message fragments with the same meaning.
const RETRYABLE_MESSAGES: [&str; 7] = [
    "rate limit",
    "too many requests",
    "temporarily unavailable",
    "timeout",
    "timed out",
    "try again",
    "overloaded",
];

impl Error {
    /// A failed wallet or preflight call to `method`.
    pub(crate) fn from_call(method: &str, error: CallError) -> Self {
        match error {
            CallError::Transport(error) => Self::from(error),
            CallError::Remote(failure) => {
                let message = failure.message.to_ascii_lowercase();
                let retryable = RETRYABLE_CODES.contains(&failure.code)
                    || RETRYABLE_MESSAGES
                        .iter()
                        .any(|needle| message.contains(needle));
                Self::rpc(
                    format!(
                        "Ethereum JSON-RPC {method} failed with code {}",
                        failure.code
                    ),
                    retryable,
                )
            }
        }
    }

    /// A failed block or receipt read for indexing.
    pub(crate) fn from_block_call(error: &CallError) -> Self {
        match error {
            CallError::Transport(error) => Self::from(error.clone()),
            CallError::Remote(failure) => Self::rpc(
                format!(
                    "Ethereum JSON-RPC request failed with code {}",
                    failure.code
                ),
                error.is_retryable(),
            ),
        }
    }

    /// A response that does not have the documented shape; never retried.
    pub(crate) fn invalid_response(method: &str, message: impl std::fmt::Display) -> Self {
        Self::rpc(
            format!("Ethereum RPC {method} returned an invalid response: {message}"),
            false,
        )
    }
}

/// A transport failure keeps its message and the transport's retry decision.
impl From<json_rpc::Error> for Error {
    fn from(error: json_rpc::Error) -> Self {
        Self::rpc(error.to_string(), error.is_retryable())
    }
}

/// Meaning of a remote failure's message, matched on ASCII lower case.
pub(crate) trait Remote {
    /// The node already has this exact transaction.
    fn is_already_known(&self) -> bool;

    /// The code of an EVM execution revert, if this is one.
    fn revert_code(&self) -> Option<i64>;
}

impl Remote for CallError {
    fn is_already_known(&self) -> bool {
        let Self::Remote(failure) = self else {
            return false;
        };
        let message = failure.message.to_ascii_lowercase();
        message.contains("already known") || message.contains("known transaction")
    }

    fn revert_code(&self) -> Option<i64> {
        let Self::Remote(failure) = self else {
            return None;
        };
        let message = failure.message.to_ascii_lowercase();
        (message.contains("execution reverted") || message.contains("execution revert"))
            .then_some(failure.code)
    }
}
