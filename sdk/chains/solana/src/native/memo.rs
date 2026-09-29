use std::fmt;

use super::{Error, ErrorKind};

/// Opaque 256-bit token carried by the Memo-v3 instruction of every transfer,
/// so two transfers with equal amounts still have distinct messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Memo([u8; Self::LENGTH]);

impl Memo {
    /// Token length in bytes.
    pub const LENGTH: usize = 32;

    /// The token with these exact bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; Self::LENGTH]) -> Self {
        Self(bytes)
    }

    /// A fresh token from operating-system randomness.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::KeyGeneration`] when no randomness is available.
    pub fn generate() -> Result<Self, Error> {
        Self::generate_with(|bytes| getrandom::fill(bytes).map_err(|_| ()))
    }

    /// The token bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::LENGTH] {
        &self.0
    }

    fn generate_with(
        mut fill: impl FnMut(&mut [u8; Self::LENGTH]) -> Result<(), ()>,
    ) -> Result<Self, Error> {
        let mut bytes = [0_u8; Self::LENGTH];
        fill(&mut bytes).map_err(|()| {
            Error::new(
                ErrorKind::KeyGeneration,
                "operating system random source is unavailable",
            )
        })?;
        Ok(Self(bytes))
    }
}

/// Canonical Base58, the exact memo instruction data.
impl fmt::Display for Memo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&bs58::encode(self.0).into_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_opaque_token_bytes() {
        let bytes = [19; Memo::LENGTH];
        assert_eq!(Memo::from_bytes(bytes).as_bytes(), &bytes);
    }

    #[test]
    fn generates_canonical_distinct_tokens_and_reports_rng_failure() {
        let first = Memo::generate_with(|bytes| {
            *bytes = [1; Memo::LENGTH];
            Ok(())
        })
        .expect("first token");
        let second = Memo::generate_with(|bytes| {
            *bytes = [2; Memo::LENGTH];
            Ok(())
        })
        .expect("second token");

        assert_ne!(first, second);
        assert_eq!(
            bs58::decode(first.to_string())
                .into_vec()
                .expect("canonical Base58 token"),
            first.as_bytes()
        );
        assert_eq!(
            Memo::generate_with(|_| Err(()))
                .expect_err("injected failure")
                .kind,
            ErrorKind::KeyGeneration
        );
    }
}
