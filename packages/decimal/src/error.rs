use std::{error, fmt};

/// A failed parse, conversion, or amount check.
///
/// One error covers every failure this crate can report: non-canonical text,
/// a negative amount where only a non-negative one is meaningful, more
/// fractional digits than the requested precision, and an atomic value that
/// does not fit a fixed integer width. Callers surface [`Error::message`];
/// none of them branch on a classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// Human-readable detail.
    pub message: String,
}

impl Error {
    /// Builds an error carrying `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn invalid() -> Self {
        Self::new("decimal must use canonical base-10 notation")
    }

    pub(crate) fn negative() -> Self {
        Self::new("currency amount must not be negative")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}
