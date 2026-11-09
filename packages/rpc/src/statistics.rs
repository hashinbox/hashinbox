//! Per-endpoint execution outcomes, readable without locks or I/O.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{CallResult, Error, ErrorKind, Failure, Limit};

const REMOTE: u64 = 1 << 32;
const HTTP: u64 = 1 << 16;

/// A shared handle to one HTTP endpoint's execution counters.
/// Cloned clients and handles observe the same counters; separate clients do not.
#[derive(Clone, Debug, Default)]
pub struct Statistics(Arc<Counters>);

#[derive(Debug, Default)]
struct Counters {
    attempts: AtomicU64,
    failures: AtomicU64,
    consecutive_failures: AtomicU64,
    last_failure: AtomicU64,
}

/// One nonblocking sample of endpoint outcomes.
/// Fields are sampled independently while requests run; after they settle all
/// outcomes are included. A cancelled in-flight attempt has no invented outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// HTTP executions begun after local parameter validation; a batch counts once.
    pub attempts: u64,
    /// Executions with a transport error or at least one JSON-RPC error response.
    pub failures: u64,
    /// Failed completions since the latest successful completion.
    pub consecutive_failures: u64,
    /// Last failure's sanitized classification or remote code, retained after success.
    /// Never contains a URL, header, request, remote message or response data.
    pub last_failure: Option<String>,
}

impl Statistics {
    /// Reads counters from memory without acquiring a lock or making a request.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let code = self.0.last_failure.load(Ordering::Relaxed);
        Snapshot {
            attempts: self.0.attempts.load(Ordering::Relaxed),
            failures: self.0.failures.load(Ordering::Relaxed),
            consecutive_failures: self.0.consecutive_failures.load(Ordering::Relaxed),
            last_failure: Self::message(code),
        }
    }

    pub(crate) fn begin(&self) {
        Self::increment(&self.0.attempts);
    }

    /// Records one call's outcome; what failed, for the log: a node's error
    /// object with its (truncated) message.
    pub(crate) fn call(&self, result: &Result<CallResult, Error>) -> Option<String> {
        let failure = match result {
            Err(error) => Some(Self::transport(error)),
            Ok(Err(failure)) => Some(Self::remote(failure)),
            Ok(Ok(_)) => None,
        };
        self.finish(failure)
    }

    /// Records one batch's outcome, failed by its first error object.
    pub(crate) fn batch(&self, result: &Result<Vec<CallResult>, Error>) -> Option<String> {
        let failure = match result {
            Err(error) => Some(Self::transport(error)),
            Ok(results) => results
                .iter()
                .find_map(|entry| entry.as_ref().err())
                .map(Self::remote),
        };
        self.finish(failure)
    }

    /// Whether the latest completion failed with one of `limits`.
    pub(crate) fn limited(&self, limits: &[Limit]) -> bool {
        if self.0.consecutive_failures.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let code = self.0.last_failure.load(Ordering::Relaxed);
        limits.iter().any(|limit| {
            code == match *limit {
                Limit::Http(status) => HTTP | u64::from(status),
                Limit::Remote(remote) => REMOTE | u64::from(remote as u32),
            }
        })
    }

    fn finish(&self, failure: Option<(u64, String)>) -> Option<String> {
        let Some((code, text)) = failure else {
            self.0.consecutive_failures.store(0, Ordering::Relaxed);
            return None;
        };
        self.0.last_failure.store(code, Ordering::Relaxed);
        Self::increment(&self.0.failures);
        Self::increment(&self.0.consecutive_failures);
        Some(text)
    }

    fn increment(counter: &AtomicU64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(1))
        });
    }

    /// A node's error object: its code, and its text with the message.
    fn remote(failure: &Failure) -> (u64, String) {
        (REMOTE | u64::from(failure.code as u32), failure.to_string())
    }

    /// A transport failure: its classification, and its text; a rate-limited
    /// answer keeps the node's code and message.
    fn transport(error: &Error) -> (u64, String) {
        let code = Self::kind(error.kind);
        let text = match error.kind {
            ErrorKind::RateLimited(_) => error.message.clone(),
            _ => Self::message(code).unwrap_or_default(),
        };
        (code, text)
    }

    fn kind(kind: ErrorKind) -> u64 {
        match kind {
            ErrorKind::InvalidConfiguration => 1,
            ErrorKind::InvalidRequest => 2,
            ErrorKind::Timeout => 3,
            ErrorKind::Unavailable => 4,
            ErrorKind::ResponseTooLarge => 5,
            ErrorKind::InvalidResponse => 6,
            ErrorKind::HttpStatus(status) => HTTP | u64::from(status),
            ErrorKind::RateLimited(code) => REMOTE | u64::from(code as u32),
        }
    }

    fn message(code: u64) -> Option<String> {
        if code == 0 {
            return None;
        }
        if code & REMOTE != 0 {
            return Some(format!(
                "JSON-RPC call failed with code {}",
                code as u32 as i32
            ));
        }
        if code & HTTP != 0 {
            return Some(format!("JSON-RPC endpoint returned HTTP {}", code as u16));
        }
        Some(
            match code {
                1 => "JSON-RPC endpoint configuration is invalid",
                2 => "JSON-RPC request is invalid",
                3 => "JSON-RPC request timed out",
                4 => "JSON-RPC transport is unavailable",
                5 => "JSON-RPC response exceeded its configured limit",
                _ => "JSON-RPC endpoint returned an invalid response",
            }
            .to_owned(),
        )
    }
}
