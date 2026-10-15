use anyhow::Context as _;
use base::{Signer, TransactionError};
use crypto::{Curve, Ed25519, PrivateKey as _, SecretBytes};

use crate::{Address, Network};

/// An Ed25519 seed and the account it signs for. `Debug` shows no secret.
pub struct KeyPair {
    pub(crate) key: Ed25519,
    address: Address,
}

impl KeyPair {
    /// A new random key on `network`.
    ///
    /// # Errors
    ///
    /// When randomness is unavailable.
    pub fn generate(network: &Network) -> anyhow::Result<Self> {
        let key = Ed25519::generate().context("Solana key generation failed")?;
        Ok(Self::new(key, network))
    }

    /// The key of a 32-byte seed, on `network`.
    ///
    /// # Errors
    ///
    /// When the bytes are not a 32-byte seed.
    pub fn import(secret: &SecretBytes, network: &Network) -> anyhow::Result<Self> {
        let key = Ed25519::from_bytes(secret.as_bytes()).context("invalid Solana key")?;
        Ok(Self::new(key, network))
    }

    /// The key of `signer`, deriving its address on `network`.
    pub(crate) fn from_signer(
        signer: &Signer,
        network: &Network,
    ) -> Result<Self, TransactionError> {
        if signer.curve != Curve::Ed25519 {
            return Err(TransactionError::failed("a Solana key is Ed25519"));
        }
        Self::import(signer.secret(), network)
            .ok()
            .filter(|key| base::Addressed::address(key) == &signer.address)
            .ok_or_else(|| TransactionError::failed("the key does not sign for its address"))
    }

    fn new(key: Ed25519, network: &Network) -> Self {
        let address = Address::new(key.public_key().into(), network);
        Self { key, address }
    }

    /// The 32-byte seed.
    #[must_use]
    pub fn secret(&self) -> SecretBytes {
        self.key.to_bytes()
    }

    /// The account it signs for.
    #[must_use]
    pub const fn native_address(&self) -> &Address {
        &self.address
    }
}

impl base::Addressed for KeyPair {
    fn address(&self) -> &base::Address {
        base::Addressed::address(&self.address)
    }
}

impl base::Networked for KeyPair {
    fn network(&self) -> &base::Network {
        base::Networked::network(&self.address)
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeyPair")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}
