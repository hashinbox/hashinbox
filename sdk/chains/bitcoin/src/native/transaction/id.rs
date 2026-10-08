use std::{fmt, str::FromStr};

use bitcoin::{Txid, hashes::Hash};

use crate::native::Error;

/// A non-witness transaction ID (txid), stored in the digest byte order of
/// `rust-bitcoin`.
///
/// `Display` and `FromStr` use Bitcoin Core's reversed hexadecimal order.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TransactionId(pub [u8; 32]);

impl fmt::Display for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        Txid::from_byte_array(self.0).fmt(formatter)
    }
}

impl fmt::Debug for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for TransactionId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<Txid>()
            .map(Self::from)
            .map_err(|_| Error::invalid_transaction("Bitcoin transaction ID is invalid"))
    }
}

impl From<Txid> for TransactionId {
    fn from(id: Txid) -> Self {
        Self(id.to_byte_array())
    }
}

impl From<TransactionId> for Txid {
    fn from(id: TransactionId) -> Self {
        Self::from_byte_array(id.0)
    }
}
