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

#[cfg(test)]
mod tests {
    use json_rpc::{ErrorKind, Failure, RawJson};

    use super::*;

    fn remote(code: i64, message: &str) -> CallError {
        CallError::Remote(Failure {
            code,
            message: message.to_owned(),
            data: Some(
                RawJson::from_serializable(&serde_json::json!({"secret":"hidden"})).unwrap(),
            ),
        })
    }

    #[test]
    fn wallet_policy_preserves_codes_ascii_substrings_and_redaction() {
        for (code, message, retryable) in [
            (-32_605, "unrelated", true),
            (-32_603, "unrelated", true),
            (-32_005, "unrelated", true),
            (-32_000, "unrelated", false),
            (429, "unrelated", false),
            (3, "execution reverted", false),
            (3, "RATE LIMIT", true),
            (3, "prefix TOO MANY REQUESTS suffix", true),
            (3, "TEMPORARILY UNAVAILABLE", true),
            (3, "timeout", true),
            (3, "timed out", true),
            (3, "try again", true),
            (3, "overloaded", true),
            (3, "rate\tlimit", false),
            (3, "tímeout", false),
        ] {
            let error = Error::from_call("eth_call", remote(code, message));
            assert_eq!(error.retryable, retryable, "{code}: {message}");
            assert_eq!(
                error.message,
                format!("Ethereum JSON-RPC eth_call failed with code {code}")
            );
        }
    }

    #[test]
    fn block_policy_retries_only_server_errors_and_redacts() {
        for (code, retryable) in [
            (-32100, false),
            (-32099, true),
            (-32000, true),
            (-31999, false),
            (-32601, false),
            (429, false),
            (3, false),
        ] {
            let error = Error::from_block_call(&remote(code, "rate limit secret provider details"));
            assert_eq!(error.retryable, retryable);
            assert_eq!(
                error.message,
                format!("Ethereum JSON-RPC request failed with code {code}")
            );
        }
    }

    #[test]
    fn transport_errors_keep_message_and_exact_retryability() {
        for (kind, retryable) in [
            (ErrorKind::InvalidConfiguration, false),
            (ErrorKind::InvalidRequest, false),
            (ErrorKind::Timeout, true),
            (ErrorKind::Unavailable, true),
            (ErrorKind::HttpStatus(429), true),
            (ErrorKind::HttpStatus(500), false),
            (ErrorKind::HttpStatus(502), true),
            (ErrorKind::HttpStatus(503), true),
            (ErrorKind::HttpStatus(504), true),
            (ErrorKind::ResponseTooLarge, false),
            (ErrorKind::InvalidResponse, false),
        ] {
            let transport = json_rpc::Error {
                kind,
                message: "transport result".to_owned(),
            };
            for error in [
                Error::from(transport.clone()),
                Error::from_call("eth_call", CallError::Transport(transport.clone())),
                Error::from_block_call(&CallError::Transport(transport)),
            ] {
                assert_eq!(error.message, "transport result");
                assert_eq!(error.retryable, retryable, "{kind:?}");
            }
        }
    }

    #[test]
    fn remote_classification_uses_only_ascii_message_matching() {
        for (message, known, revert) in [
            ("ALREADY KNOWN", true, false),
            ("prefix Known Transaction suffix", true, false),
            ("unknown transaction", true, false),
            ("EXECUTION REVERTED: reason", false, true),
            ("prefix execution revert suffix", false, true),
            ("execution failed", false, false),
            ("already\tknown", false, false),
            ("ÉXECUTION REVERTED", false, false),
        ] {
            let error = remote(-32_000, message);
            assert_eq!(error.is_already_known(), known);
            assert_eq!(error.revert_code(), revert.then_some(-32_000));
        }
        let local = CallError::Transport(json_rpc::Error {
            kind: ErrorKind::Timeout,
            message: "already known execution reverted".to_owned(),
        });
        assert!(!local.is_already_known());
        assert_eq!(local.revert_code(), None);
    }

    #[test]
    fn invalid_response_keeps_method_context_and_is_terminal() {
        let error = Error::invalid_response("eth_call", "hex data has an invalid length");
        assert_eq!(
            error.message,
            "Ethereum RPC eth_call returned an invalid response: hex data has an invalid length"
        );
        assert!(!error.retryable);
    }
}
