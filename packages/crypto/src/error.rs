use std::{error, fmt};

/// A cryptographic failure; the message never contains key material.
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
    /// The operating system random source is unavailable.
    KeyGeneration,
    /// Secret bytes are not a valid key on the curve they were imported for.
    InvalidKey,
    /// The digest is not exactly 32 bytes.
    InvalidDigest,
    /// The tweak is malformed or produces an invalid key.
    InvalidTweak,
    /// The requested signature encoding or public-key format does not exist
    /// on the curve.
    UnsupportedEncoding,
    /// The signing primitive failed.
    Signing,
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

/// A signer refused or failed a [`crate::SignRequest`].
///
/// [`Error`] classifies a primitive that failed; this classifies a request
/// that was refused, including one refused because a primitive rejected it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SignerError {
    /// Stable classification.
    pub kind: SignerErrorKind,
    /// Human-readable detail; never contains key material.
    pub message: String,
}

/// Why a signer refused or failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SignerErrorKind {
    /// The signer's key is on a different curve.
    UnsupportedCurve,
    /// The signer cannot produce the requested signature scheme.
    UnsupportedScheme,
    /// The request shape is refused, e.g. a raw message or an ECDSA key tweak.
    UnsupportedOperation,
    /// The digest, encoding, tweak, or key material is invalid.
    InvalidRequest,
    /// Any other signer failure.
    Other,
}

impl SignerError {
    pub(crate) fn new(kind: SignerErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// A primitive rejected the request; the primitive's message is kept as-is.
    pub(crate) fn from_crypto(error: Error) -> Self {
        Self::new(SignerErrorKind::InvalidRequest, error.to_string())
    }
}

impl fmt::Display for SignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for SignerError {}
