#![doc = include_str!("../README.md")]

mod macros;
mod schema;
mod scope;
mod scopes;

use std::fmt;

pub use schema::{Resource, Schema};
pub use scope::{Action, Scope, Segment};
pub use scope_macros::{require, utoipa};
pub use scopes::Scopes;

/// What a caller was granted: the scopes [`require!`] checks against. A session, a key or plain [`Scopes`].
pub trait Granted {
    /// The scopes it holds.
    fn scopes(&self) -> &Scopes;
}

impl Granted for Scopes {
    fn scopes(&self) -> &Scopes {
        self
    }
}

impl<T: Granted + ?Sized> Granted for &T {
    fn scopes(&self) -> &Scopes {
        (**self).scopes()
    }
}

/// Why a scope's text does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// No step before the action.
    Empty,
    /// A resource or action name that is not ASCII letters, digits, `_`
    /// or `-`.
    Name(String),
    /// An id not closed by `]`, or empty.
    Id(String),
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
            Self::Id(text) => write!(out, "bad id in `{text}`"),
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
    needed: String,
}

impl Denied {
    /// Refused for want of `needed`.
    #[must_use]
    pub fn new(needed: Scope) -> Self {
        Self {
            needed: needed.to_string(),
        }
    }

    /// The text of the scope it needed: a scope's, or a `require!`
    /// template's filled text that does not parse.
    #[must_use]
    pub fn needed(&self) -> &str {
        &self.needed
    }
}

impl fmt::Display for Denied {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "missing scope {}", self.needed)
    }
}

impl std::error::Error for Denied {}

/// `id` escaped as a scope writes it between `[` and `]`: what
/// [`require!`] puts in a template's `{name}`.
#[doc(hidden)]
#[must_use]
pub fn __escape_id(id: &str) -> String {
    scope::escape(id)
}

/// [`require!`]'s check: `text` parsed, then covered by `granted`. Text
/// that does not parse is denied.
///
/// # Errors
///
/// [`Denied`] when the check fails.
#[doc(hidden)]
pub fn __require(granted: &Scopes, text: String) -> Result<(), Denied> {
    let Ok(needed) = text.parse::<Scope>() else {
        return Err(Denied { needed: text });
    };
    if granted.covers(&needed) {
        return Ok(());
    }
    Err(Denied::new(needed))
}
