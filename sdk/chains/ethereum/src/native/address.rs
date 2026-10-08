use std::{fmt, str::FromStr};

use super::{Error, ErrorKind};

/// A 20-byte Ethereum account or contract address.
///
/// `Display` is the canonical lowercase `0x` form. Parsing accepts all-lower
/// and all-upper hex, and mixed case only with a valid EIP-55 checksum.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Address([u8; 20]);

impl Address {
    /// The zero address, used by ERC-20 for mints and burns.
    pub const ZERO: Self = Self([0; 20]);

    /// The address these exact 20 bytes name.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// The exact 20 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Whether this is the zero address.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self == &Self::ZERO
    }

    fn invalid(message: &str) -> Error {
        Error::new(ErrorKind::InvalidAddress, message)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Address {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let hexadecimal = input
            .strip_prefix("0x")
            .ok_or_else(|| Self::invalid("Ethereum address is missing its 0x prefix"))?;
        if hexadecimal.len() != 40 {
            return Err(Self::invalid(
                "Ethereum address must contain exactly 20 bytes",
            ));
        }
        let mut bytes = [0_u8; 20];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hexadecimal[index * 2..index * 2 + 2], 16)
                .map_err(|_| Self::invalid("Ethereum address contains invalid hexadecimal"))?;
        }
        let address = Self(bytes);
        let mixed_case = hexadecimal.chars().any(|c| c.is_ascii_lowercase())
            && hexadecimal.chars().any(|c| c.is_ascii_uppercase());
        if mixed_case && alloy_primitives::Address::from(bytes).to_checksum(None) != input {
            return Err(Self::invalid(
                "Ethereum address has an invalid EIP-55 checksum",
            ));
        }
        Ok(address)
    }
}
