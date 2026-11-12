//! Why a trigger cannot be stored: one [`Error`], its [`Class`] saying what
//! kind of thing is wrong, its message saying what exactly.

use std::fmt;

/// What kind of thing is wrong. The server's API answers 404 for
/// [`Class::NotConfigured`], 400 for the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// A selection that picks nothing or everything: an empty id list, an
    /// empty "any of" or "all of", an empty asset id. 400.
    EmptySelection,
    /// A name that is not well formed, such as a tag. 400.
    Malformed,
    /// A condition that can never hold: an empty "any of". 400.
    Impossible,
    /// A "not" right inside a "not": it cancels out. 400.
    Redundant,
    /// A value out of its range: a count against a fractional number, a
    /// factor of zero or less. 400.
    OutOfRange,
    /// A filter condition that never reads the wallet it tests: it keeps
    /// all or none, so it belongs in the check. 400.
    FilterIgnoresWallet,
    /// No action of the name is registered. 400.
    UnknownAction,
    /// An action's input does not decode. 400.
    BadInput,
    /// An action refuses its input. 400.
    Refused,
    /// An action's input names something not configured here, such as an
    /// asset. 404.
    NotConfigured,
}

/// Why a trigger, or a part of it, cannot be stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What kind of thing is wrong.
    pub class: Class,
    message: String,
}

impl Error {
    /// A `class` error saying `message`.
    #[must_use]
    pub fn new(class: Class, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    /// What exactly is wrong, as the API shows it.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
