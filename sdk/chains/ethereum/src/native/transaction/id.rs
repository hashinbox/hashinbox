use std::{fmt, str::FromStr};

use crate::native::{Error, ErrorKind};

/// A 32-byte Ethereum transaction hash: the Keccak-256 of its signed envelope.
///
/// `Display` and `FromStr` use the canonical lowercase `0x` form.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionId([u8; 32]);

impl TransactionId {
    /// The transaction hash these exact 32 bytes are.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The exact 32 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn invalid(message: &str) -> Error {
        Error::new(ErrorKind::InvalidTransaction, message)
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for TransactionId {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let hexadecimal = input
            .strip_prefix("0x")
            .ok_or_else(|| Self::invalid("Ethereum transaction ID is missing its 0x prefix"))?;
        if hexadecimal.len() != 64 {
            return Err(Self::invalid(
                "Ethereum transaction ID must contain exactly 32 bytes",
            ));
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&hexadecimal[index * 2..index * 2 + 2], 16).map_err(|_| {
                    Self::invalid("Ethereum transaction ID contains non-hex characters")
                })?;
        }
        let parsed = Self(bytes);
        if parsed.to_string() != input {
            return Err(Self::invalid(
                "Ethereum transaction ID must use canonical lowercase hexadecimal",
            ));
        }
        Ok(parsed)
    }
}
