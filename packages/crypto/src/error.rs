use std::{error, fmt};

/// A key import, signing, randomness or hashing failure; the message never
/// contains key material, a password or a mnemonic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What went wrong, for callers to branch on.
    pub kind: ErrorKind,
    /// Human-readable context.
    pub message: String,
}

/// Classification of an [`Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bytes or text are not a valid key on the curve.
    InvalidKey,
    /// The signing primitive rejected the digest or tweak.
    Signing,
    /// The operating-system random source is unavailable.
    Random,
    /// The password hash (argon2) refused its input.
    Hashing,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}
