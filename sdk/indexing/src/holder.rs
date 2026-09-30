//! Whose balance, history or observation a call is about.

use crate::{Address, AssetId, IndexError, IndexErrorKind};

/// One address, in one asset: whose history, balance or observation a call is
/// about.
///
/// `wallets::Wallet` is one; so is a watched address with no key, which is
/// what [`Subject`] is for. It lives here rather than in `wallets` because it
/// is the query input, and `wallets` may depend on `indexing` while the
/// reverse is forbidden.
pub trait Holder: Send + Sync {
    /// The address.
    fn address(&self) -> &Address;

    /// The asset it is read in; its chain is `address().network.chain`.
    fn asset(&self) -> &AssetId;

    /// Rejects a holding no store can hold rows for: a blank
    /// [`address`](Self::address), or an [`asset`](Self::asset) of another chain
    /// than it. The checks every query over a holding owes, stated here rather
    /// than restated by each of them — or by each store that answers them —
    /// because a malformed holding is no one reader's concern.
    ///
    /// A request that carries more than a holding overrides this and adds its
    /// own: [`HistoryQuery`](crate::HistoryQuery) also bounds its page.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidRequest`] for a blank address or an asset of
    /// another chain.
    fn validate(&self) -> Result<(), IndexError> {
        require_holding(self.address(), self.asset())
    }
}

/// Rejects a holding no store can hold rows for, which is
/// [`Holder::validate`]'s whole body — a free function so that a request type
/// carrying more than a holding can state this half and then add its own.
pub(crate) fn require_holding(address: &Address, asset: &AssetId) -> Result<(), IndexError> {
    require_address(address)?;
    if asset.chain == address.network.chain {
        return Ok(());
    }
    Err(IndexError::new(
        IndexErrorKind::InvalidRequest,
        "the asset belongs to another chain than the address",
        false,
    ))
}

/// Rejects an address no store can hold a row for.
///
/// Blankness only: which network an address belongs to is carried by the
/// address, and one store holds every network. A blank address would key rows
/// nothing can ever read back, and refusing it here is half of why a pass never
/// has to check the selection it reads (`tests/contracts/watch.rs`'s `WatchContract`).
pub(crate) fn require_address(address: &Address) -> Result<(), IndexError> {
    if address.value.trim().is_empty() {
        return Err(IndexError::new(
            IndexErrorKind::InvalidRequest,
            "an indexed address must not be blank",
            false,
        ));
    }
    Ok(())
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
