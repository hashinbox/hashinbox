#![doc = include_str!("../README.md")]

mod macros;
mod schema;
mod scope;
mod scopes;

use std::fmt;

pub use schema::{Resource, Schema};
pub use scope::{Action, Scope, Segment};
pub use scopes::Scopes;

/// Why a scope's text does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// No step before the action.
    Empty,
    /// A resource or action name that is not ASCII letters, digits, `_`
    /// or `-`.
    Name(String),
    /// Ids not closed by `]`, empty, or one of them empty.
    Ids(String),
    /// No `$action` at the end.
    Action(String),
    /// A `%` not followed by two hex digits, or an id not UTF-8.
    Escape(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => out.write_str("a scope needs a resource"),
            Self::Name(name) => write!(out, "`{name}` is not a name"),
            Self::Ids(text) => write!(out, "bad ids in `{text}`"),
            Self::Action(text) => write!(out, "`{text}` does not end with `$action`"),
            Self::Escape(text) => write!(out, "bad escape in `{text}`"),
        }
    }
}

impl std::error::Error for ParseError {}

/// A request refused: the scope it needed and was not granted
/// ([`require!`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied {
    needed: Scope,
}

impl Denied {
    /// Refused for want of `needed`.
    #[must_use]
    pub const fn new(needed: Scope) -> Self {
        Self { needed }
    }

    /// The scope it needed.
    #[must_use]
    pub const fn needed(&self) -> &Scope {
        &self.needed
    }
}

impl fmt::Display for Denied {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "missing scope {}", self.needed)
    }
}

impl std::error::Error for Denied {}
