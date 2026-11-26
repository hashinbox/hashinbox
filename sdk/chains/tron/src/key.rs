use anyhow::Context as _;
use base::{Signer, TransactionError};
use crypto::{Curve, PrivateKey as _, Secp256k1Ecdsa, SecretBytes};

use crate::{Account, Address, Network};

/// A secp256k1 secret and the account it signs for. `Debug` shows no secret.
pub struct KeyPair {
    pub(crate) key: Secp256k1Ecdsa,
    address: Address,
}

impl KeyPair {
    /// A new random key on `network`.
    ///
    /// # Errors
    ///
    /// When randomness is unavailable.
    pub fn generate(network: &Network) -> anyhow::Result<Self> {
        let key = Secp256k1Ecdsa::generate().context("Tron key generation failed")?;
        Ok(Self::new(key, network))
    }

    /// The key of 32 secret bytes, on `network`.
    ///
    /// # Errors
    ///
    /// When the bytes are not a secp256k1 key.
    pub fn import(secret: &SecretBytes, network: &Network) -> anyhow::Result<Self> {
        let key = Secp256k1Ecdsa::from_bytes(secret.as_bytes()).context("invalid Tron key")?;
        Ok(Self::new(key, network))
    }

    /// The key of `signer`, deriving its address on `network`.
    pub(crate) fn from_signer(
        signer: &Signer,
        network: &Network,
    ) -> Result<Self, TransactionError> {
        if signer.curve != Curve::Secp256k1 {
            return Err(TransactionError::failed("a Tron key is secp256k1"));
        }
        Self::import(signer.secret(), network)
            .ok()
            .filter(|key| base::Addressed::address(key) == &signer.address)
            .ok_or_else(|| TransactionError::failed("the key does not sign for its address"))
    }

    fn new(key: Secp256k1Ecdsa, network: &Network) -> Self {
        let address = Address::new(Account::from_key(&key), network);
        Self { key, address }
    }

    /// The secret bytes.
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
