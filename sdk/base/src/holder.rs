//! Whose balance, history or observation a call is about.

use crate::{Address, AssetId};

/// One address, in one asset: whose history, balance or observation a call is
/// about.
///
/// A wallet is one; so is a watched address with no key, which is what
/// [`Subject`] is for. It lives in `base` so that both `wallets` and
/// `indexing` can name it without depending on each other.
pub trait Holder: Send + Sync {
    /// The address.
    fn address(&self) -> &Address;

    /// The asset it is read in; its chain is `address().network.chain`.
    fn asset(&self) -> &AssetId;
}

/// The subject of a watch or a read with no wallet behind it: one address and
/// the asset it is read in.
///
/// Deliberately not called a "holding": in a payments SDK that reads as a
/// balance position, and the word already had another meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    /// The observed address.
    pub address: Address,
    /// The asset its history and balance are read in.
    pub asset: AssetId,
}

impl Holder for Subject {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &AssetId {
        &self.asset
    }
}
