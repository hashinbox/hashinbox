use zeroize::Zeroize;

/// Opaque in-memory secret material passed from composition into a key owner.
///
/// It intentionally implements neither `Clone`, `Debug`, `Display`, nor Serde.
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Takes ownership of secret bytes; they are zeroized on drop.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Borrows the secret; the only way to read it.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Decodes hexadecimal text (either case, no prefix) into secret bytes.
    /// The caller checks the length and case its key format requires.
    ///
    /// Fallible like every other constructor here, so the one documented
    /// secret parse composes with `?` wherever a caller already handles
    /// [`Error`](crate::Error). An [`Option`] did not, which is why callers
    /// reached for a general-purpose hex decoder instead.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidKey`](crate::ErrorKind::InvalidKey) for odd-length
    /// or non-hexadecimal text. The message quotes nothing back.
    pub fn from_hex(text: &str) -> Result<Self, crate::Error> {
        let invalid = || {
            crate::Error::new(
                crate::ErrorKind::InvalidKey,
                "secret key text is not an even number of hexadecimal digits",
            )
        };
        let digits = text.as_bytes();
        if !digits.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let mut bytes = Self(Vec::with_capacity(digits.len() / 2));
        for pair in digits.chunks_exact(2) {
            let digit = |byte: u8| char::from(byte).to_digit(16).ok_or_else(invalid);
            let (high, low) = (digit(pair[0])?, digit(pair[1])?);
            bytes
                .0
                .push(u8::try_from(high << 4 | low).map_err(|_| invalid())?);
        }
        Ok(bytes)
    }

    /// Generates a valid secp256k1 scalar from the operating system CSPRNG.
    ///
    /// Key generation lives in the generic cryptography package so wallet
    /// applications never manufacture or serialize private-key bytes. Random
    /// bytes outside the curve's group order are rejected and drawn again, by
    /// the very key type [`crate::KeyPair::secp256k1`] imports into.
    pub fn generate_secp256k1() -> Result<Self, crate::Error> {
        loop {
            let candidate = Self::random()?;
            if crate::secret_key::SecretScalar::new(candidate.as_bytes()).is_ok() {
                return Ok(candidate);
            }
        }
    }

    /// Generates a 32-byte Ed25519 seed from the operating system CSPRNG.
    ///
    /// The opaque result can be handed directly to an SDK wallet factory or
    /// durable wallet registry without exposing it through formatting or
    /// serialization traits.
    ///
    /// Nothing is rejected here, unlike secp256k1: Ed25519 hashes its seed
    /// into a scalar, so every 32-byte string is a valid seed. The seed still
    /// passes through the key type [`crate::KeyPair::ed25519`] imports into,
    /// so "valid" means the same thing on the generated and imported paths.
    pub fn generate_ed25519() -> Result<Self, crate::Error> {
        let candidate = Self::random()?;
        crate::secret_key::SecretSeed::new(candidate.as_bytes())?;
        Ok(candidate)
    }

    /// 32 bytes from the operating system CSPRNG, on no curve yet.
    fn random() -> Result<Self, crate::Error> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| {
            crate::Error::new(
                crate::ErrorKind::KeyGeneration,
                "operating system random source is unavailable",
            )
        })?;
        Ok(Self::new(bytes))
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
