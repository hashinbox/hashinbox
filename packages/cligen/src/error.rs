//! Why a document cannot be turned into clients.

use std::fmt;

/// Why [`load`](crate::load) or [`generate`](crate::generate) failed. Each
/// names where: a JSON pointer into the document, an operation or a
/// generated name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text is not JSON, or not an OpenAPI 3.1 document.
    Parse(String),
    /// A feature cligen does not support, at a JSON pointer.
    Unsupported {
        /// Where: `#/components/schemas/Pet/anyOf`.
        at: String,
        /// What: `anyOf`.
        feature: String,
    },
    /// A document cligen cannot read as meant: an unknown `$ref`, success
    /// responses of different types.
    Invalid {
        /// Where, as a JSON pointer.
        at: String,
        /// Why.
        reason: String,
    },
    /// A scope template's `{hole}` that names no path parameter.
    Hole {
        /// The operation's `operationId`.
        operation: String,
        /// The hole's name.
        hole: String,
    },
    /// Two things that get one name in the generated code.
    Collision {
        /// Where: a module, or the whole package.
        within: String,
        /// The generated name.
        name: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(reason) => write!(out, "not an OpenAPI 3.1 document: {reason}"),
            Self::Unsupported { at, feature } => write!(out, "{at}: `{feature}` is not supported"),
            Self::Invalid { at, reason } => write!(out, "{at}: {reason}"),
            Self::Hole { operation, hole } => write!(
                out,
                "operation `{operation}`: scope hole `{{{hole}}}` names no path parameter"
            ),
            Self::Collision { within, name } => {
                write!(out, "{within}: two things are named `{name}`")
            }
        }
    }
}

impl std::error::Error for Error {}

/// A result of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// [`Error::Unsupported`] at `at`.
pub(crate) fn unsupported<T>(at: &str, feature: &str) -> Result<T> {
    Err(Error::Unsupported {
        at: at.to_owned(),
        feature: feature.to_owned(),
    })
}

/// [`Error::Invalid`] at `at`.
pub(crate) fn invalid<T>(at: &str, reason: impl Into<String>) -> Result<T> {
    Err(Error::Invalid {
        at: at.to_owned(),
        reason: reason.into(),
    })
}
