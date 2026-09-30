//! Per-endpoint execution outcomes, readable without locks or I/O.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{CallResult, Error, ErrorKind};

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

    pub(crate) fn call(&self, result: &Result<CallResult, Error>) {
        let failure = match result {
            Err(error) => Some(Self::transport(error.kind)),
            Ok(Err(failure)) => Some(REMOTE | u64::from(failure.code as u32)),
            Ok(Ok(_)) => None,
        };
        self.finish(failure);
    }

    pub(crate) fn batch(&self, result: &Result<Vec<CallResult>, Error>) {
        let failure = match result {
            Err(error) => Some(Self::transport(error.kind)),
            Ok(results) => results
                .iter()
                .find_map(|entry| entry.as_ref().err())
                .map(|failure| REMOTE | u64::from(failure.code as u32)),
        };
        self.finish(failure);
    }

    fn finish(&self, failure: Option<u64>) {
        if let Some(code) = failure {
            self.0.last_failure.store(code, Ordering::Relaxed);
            Self::increment(&self.0.failures);
            Self::increment(&self.0.consecutive_failures);
        } else {
            self.0.consecutive_failures.store(0, Ordering::Relaxed);
        }
    }

    fn increment(counter: &AtomicU64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(1))
        });
    }

    fn transport(kind: ErrorKind) -> u64 {
        match kind {
            ErrorKind::InvalidConfiguration => 1,
            ErrorKind::InvalidRequest => 2,
            ErrorKind::Timeout => 3,
            ErrorKind::Unavailable => 4,
            ErrorKind::ResponseTooLarge => 5,
            ErrorKind::InvalidResponse => 6,
            ErrorKind::HttpStatus(status) => HTTP | u64::from(status),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Failure, RawJson};

    #[test]
    fn outcomes_reset_consecutive_failures_without_losing_last_failure() {
        let statistics = Statistics::default();
        statistics.begin();
        statistics.call(&Ok(Err(Failure {
            code: -32001,
            message: "secret node prose".into(),
            data: None,
        })));
        statistics.begin();
        statistics.batch(&Ok(vec![
            Ok(RawJson(b"null".to_vec())),
            Err(Failure {
                code: -32603,
                message: "secret batch prose".into(),
                data: None,
            }),
        ]));
        let failed = statistics.snapshot();
        assert_eq!(
            (
                failed.attempts,
                failed.failures,
                failed.consecutive_failures
            ),
            (2, 2, 2)
        );
        assert_eq!(
            failed.last_failure.as_deref(),
            Some("JSON-RPC call failed with code -32603")
        );
        statistics.begin();
        statistics.call(&Ok(Ok(RawJson(b"null".to_vec()))));
        let recovered = statistics.snapshot();
        assert_eq!(
            (
                recovered.attempts,
                recovered.failures,
                recovered.consecutive_failures
            ),
            (3, 2, 0)
        );
        assert_eq!(recovered.last_failure, failed.last_failure);
        assert!(!format!("{recovered:?}").contains("secret"));
    }

    #[test]
    fn concurrent_clones_count_every_outcome_and_distinct_handles_are_isolated() {
        let statistics = Statistics::default();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let observed = statistics.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        observed.begin();
                        observed.call(&Err(Error::new(ErrorKind::Timeout, "hidden")));
                    }
                });
            }
        });
        let snapshot = statistics.snapshot();
        assert_eq!(
            (
                snapshot.attempts,
                snapshot.failures,
                snapshot.consecutive_failures
            ),
            (8000, 8000, 8000)
        );
        assert_eq!(Statistics::default().snapshot(), Snapshot::default());
    }
}
