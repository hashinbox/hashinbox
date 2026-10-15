use anyhow::Context as _;
use base::{Signer, TransactionError};
use bitcoin::{CompressedPublicKey, XOnlyPublicKey};
use crypto::{Curve, PrivateKey as _, Secp256k1Ecdsa, SecretBytes};

use crate::{Address, AddressType, Network};

/// A secp256k1 secret, its compressed public key, and the address it
/// derives. `Debug` shows no secret.
pub struct KeyPair {
    pub(crate) key: Secp256k1Ecdsa,
    pub(crate) public: CompressedPublicKey,
    address: Address,
}

impl KeyPair {
    /// A new random key deriving a `kind` address of `network`.
    ///
    /// # Errors
    ///
    /// When randomness is unavailable.
    pub fn generate(network: Network, kind: AddressType) -> anyhow::Result<Self> {
        let key = Secp256k1Ecdsa::generate().context("Bitcoin key generation failed")?;
        Self::new(key, network, kind)
    }

    /// The key of 32 secret bytes, deriving a `kind` address of `network`.
    ///
    /// # Errors
    ///
    /// When the bytes are not a secp256k1 key.
    pub fn import(
        secret: &SecretBytes,
        network: Network,
        kind: AddressType,
    ) -> anyhow::Result<Self> {
        let key = Secp256k1Ecdsa::from_bytes(secret.as_bytes()).context("invalid Bitcoin key")?;
        Self::new(key, network, kind)
    }

    /// The key of `signer`, deriving its address (of either type) on
    /// `network`.
    pub(crate) fn from_signer(signer: &Signer, network: Network) -> Result<Self, TransactionError> {
        if signer.curve != Curve::Secp256k1 {
            return Err(TransactionError::failed("a Bitcoin key is secp256k1"));
        }
        [AddressType::SegwitV0, AddressType::Taproot]
            .into_iter()
            .filter_map(|kind| Self::import(signer.secret(), network, kind).ok())
            .find(|key| base::Addressed::address(key) == &signer.address)
            .ok_or_else(|| TransactionError::failed("the key does not sign for its address"))
    }

    fn new(key: Secp256k1Ecdsa, network: Network, kind: AddressType) -> anyhow::Result<Self> {
        let public = CompressedPublicKey::from_slice(&key.compressed_public_key())
            .context("invalid Bitcoin public key")?;
        let address = Address::from_public_key(&public, kind, network);
        Ok(Self {
            key,
            public,
            address,
        })
    }

    /// The secret bytes.
    #[must_use]
    pub fn secret(&self) -> SecretBytes {
        self.key.to_bytes()
    }

    /// The untweaked x-only public key.
    #[must_use]
    pub fn x_only(&self) -> XOnlyPublicKey {
        XOnlyPublicKey::from(self.public.0)
    }

    /// The address it derives.
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
