use std::{fmt, str::FromStr};

use solana_signature::Signature;

use crate::native::{Error, ErrorKind};

/// A transaction's identity: its first (fee payer's) Ed25519 signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionId(Signature);

impl TransactionId {
    /// The signature this ID is.
    #[must_use]
    pub const fn signature(&self) -> &Signature {
        &self.0
    }
}

impl From<Signature> for TransactionId {
    fn from(signature: Signature) -> Self {
        Self(signature)
    }
}

impl fmt::Display for TransactionId {
    /// Canonical Base58 of the signature.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for TransactionId {
    type Err = Error;

    /// Accepts only canonical Base58 of a 64-byte signature.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            Error::new(
                ErrorKind::InvalidTransaction,
                "Solana transaction ID must be a canonical Base58 signature",
            )
        };
        let signature = Signature::from_str(input).map_err(|_| invalid())?;
        if signature.to_string() != input {
            return Err(invalid());
        }
        Ok(Self(signature))
    }
}
