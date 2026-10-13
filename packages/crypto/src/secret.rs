use std::fmt;

use zeroize::Zeroizing;

/// Secret bytes, zeroized on drop. No `Clone`, `Display` or Serde; `Debug`
/// is redacted.
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    /// Takes ownership of secret bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(Zeroizing::new(bytes.into()))
    }

    /// Borrows the secret; the only way to read it.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Decodes hexadecimal text (either case, no prefix). The caller checks
    /// the length its key format requires.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidKey`](crate::ErrorKind::InvalidKey) for text that
    /// is not hexadecimal. The message quotes nothing back.
    pub fn from_hex(text: &str) -> Result<Self, crate::Error> {
        hex::decode(text).map(Self::new).map_err(|_| {
            crate::Error::new(
                crate::ErrorKind::InvalidKey,
                "secret key text is not an even number of hexadecimal digits",
            )
        })
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBytes(..)")
    }
}
