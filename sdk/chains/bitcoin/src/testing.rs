//! Test support (feature `testing`): the [`fixtures`] the crate's tests
//! share.

pub mod fixtures {
    //! Regtest keys from a seed.

    use base::Addressed as _;
    use crypto::SecretBytes;

    use crate::{AddressType, KeyPair, Network};

    /// The regtest native segwit key of 32 bytes of `seed`.
    ///
    /// # Panics
    /// Never: any 32 nonzero bytes are a key.
    #[must_use]
    pub fn key_of(seed: u8) -> KeyPair {
        let secret = SecretBytes::new([seed; 32]);
        KeyPair::import(&secret, Network::Regtest, AddressType::SegwitV0).expect("valid key")
    }

    /// [`key_of`]'s address.
    #[must_use]
    pub fn address_of(seed: u8) -> base::Address {
        key_of(seed).address().clone()
    }
}
