use std::{borrow::Cow, fmt};

/// Chain family identity, such as `bitcoin`, `ethereum`, or `solana`.
///
/// Persisted and compared as its exact text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chain(Cow<'static, str>);

impl Chain {
    /// Lets a chain crate declare its family as a `const`.
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// Decodes a stored or transported family name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(Cow::Owned(name.into()))
    }

    /// The exact family text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
