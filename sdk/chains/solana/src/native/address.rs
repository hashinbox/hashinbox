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

#[cfg(test)]
mod tests {
    use super::*;

    fn message(input: &str) -> String {
        input.parse::<Address>().unwrap_err().message
    }

    #[test]
    fn preserves_all_address_bytes() {
        let bytes = [7; solana_address::ADDRESS_BYTES];
        assert_eq!(Address::from_bytes(bytes).as_bytes(), &bytes);
    }

    #[test]
    fn renders_every_byte_as_canonical_plain_base58() {
        for bytes in [
            [0; solana_address::ADDRESS_BYTES],
            [7; solana_address::ADDRESS_BYTES],
            [u8::MAX; solana_address::ADDRESS_BYTES],
        ] {
            let address = Address::from_bytes(bytes);
            let rendered = address.to_string();

            assert_eq!(rendered, NativeAddress::from(bytes).to_string());
            assert_eq!(
                rendered
                    .parse::<Address>()
                    .expect("canonical rendering must parse"),
                address
            );
        }
    }

    #[test]
    fn parses_canonical_minimum_and_maximum_values() {
        let minimum = "11111111111111111111111111111111"
            .parse::<Address>()
            .expect("canonical zero bytes must parse");
        assert_eq!(minimum.as_bytes(), &[0; 32]);

        let maximum_bytes = [u8::MAX; solana_address::ADDRESS_BYTES];
        let maximum_text = NativeAddress::from(maximum_bytes).to_string();
        assert_eq!(maximum_text.len(), 44);
        let maximum = maximum_text
            .parse::<Address>()
            .expect("canonical maximum bytes must parse");
        assert_eq!(maximum.as_bytes(), &maximum_bytes);
    }

    #[test]
    fn rejects_invalid_base58_alphabet() {
        for input in [
            "0",
            "O1111111111111111111111111111111",
            "I1111111111111111111111111111111",
            "l1111111111111111111111111111111",
            " 11111111111111111111111111111111",
        ] {
            assert_eq!(message(input), "Solana address contains invalid Base58");
        }
    }

    #[test]
    fn rejects_empty_and_wrong_length_values() {
        for input in [
            "",
            "1111111111111111111111111111111",
            "111111111111111111111111111111111",
            "111111111111111111111111111111111111111111111",
        ] {
            let error = input.parse::<Address>().unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidAddress);
            assert_eq!(
                error.message,
                "Solana address must contain exactly 32 bytes"
            );
        }
    }

    #[test]
    fn rejects_a_decode_reencode_mismatch() {
        let input = "11111111111111111111111111111111";
        let different_value = NativeAddress::from([1; solana_address::ADDRESS_BYTES]);
        assert_eq!(
            Address::from_native(input, different_value)
                .unwrap_err()
                .message,
            "Solana address is not canonical Base58"
        );
    }

    #[test]
    fn accepts_canonical_off_curve_values() {
        let native = (0..=u8::MAX)
            .map(|byte| NativeAddress::from([byte; solana_address::ADDRESS_BYTES]))
            .find(|address| !address.is_on_curve())
            .expect("the deterministic fixture range must contain an off-curve address");
        let input = native.to_string();
        let parsed = input
            .parse::<Address>()
            .expect("canonical off-curve addresses remain readable");

        assert_eq!(parsed.as_bytes(), native.as_array());
        assert!(!parsed.is_on_curve());
    }
}
