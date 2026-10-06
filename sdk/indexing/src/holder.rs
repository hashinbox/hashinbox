//! The checks every query over a holding owes: a non-blank address and an
//! asset of the address's own chain. Free functions, because [`crate::Holder`]
//! lives in `base`, which knows no [`IndexError`].

use crate::{Address, AssetId, IndexError, IndexErrorKind};

/// Rejects a holding no store can hold rows for: a blank address, or an asset
/// of another chain than it. A request type carrying more than a holding states
/// this half and then adds its own.
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
