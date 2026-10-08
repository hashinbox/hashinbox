use std::{fmt, str::FromStr};

use solana_address::{Address as NativeAddress, error::ParseAddressError};

use super::{Error, ErrorKind};

/// Exact 32-byte Solana account identity.
///
/// Parsing and rendering accept only canonical plain Base58. Off-curve
/// (program-derived) addresses are valid identities too; whether native SOL
/// may be sent to one is a transfer rule, not an address rule.
///
/// # Examples
///
/// ```
/// use chain_solana::Address;
///
/// let address = "11111111111111111111111111111111"
///     .parse::<Address>()
///     .expect("the canonical zero address must parse");
/// assert_eq!(address.as_bytes(), &[0; 32]);
/// assert_eq!(address.to_string(), "11111111111111111111111111111111");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(NativeAddress);

impl Address {
    /// The address with these exact bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; solana_address::ADDRESS_BYTES]) -> Self {
        Self(bytes.into())
    }

    /// The 32 address bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; solana_address::ADDRESS_BYTES] {
        self.0.as_array()
    }

    /// Whether the address is an Ed25519 point, so a private key can control it.
    #[must_use]
    pub fn is_on_curve(&self) -> bool {
        self.0.is_on_curve()
    }

    pub(crate) const fn native(&self) -> &NativeAddress {
        &self.0
    }

    /// `native`, provided `input` is its canonical rendering.
    fn from_native(input: &str, native: NativeAddress) -> Result<Self, Error> {
        if native.to_string() != input {
            return Err(Self::invalid("Solana address is not canonical Base58"));
        }
        Ok(Self(native))
    }

    fn invalid(message: &str) -> Error {
        Error::new(ErrorKind::InvalidAddress, message)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Address {
    type Err = Error;

    /// Accepts only canonical Base58 of exactly 32 bytes.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let native = input
            .parse::<NativeAddress>()
            .map_err(|error| match error {
                ParseAddressError::WrongSize => {
                    Self::invalid("Solana address must contain exactly 32 bytes")
                }
                ParseAddressError::Invalid => {
                    Self::invalid("Solana address contains invalid Base58")
                }
            })?;
        Self::from_native(input, native)
    }
}
