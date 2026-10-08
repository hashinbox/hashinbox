//! [`UtxoSource`]: where a send's candidate inputs come from.

use std::sync::Arc;

use wallets::FutureResult;

use crate::native::{Address, block::Utxo};

/// Where a send's candidate inputs come from. Selection itself is pure
/// (`Builder`); this only supplies the UTXOs, read once per send at
/// reservation time. Named like `indexing::BlockSource`: one side supplies
/// blocks to the index, the other supplies inputs to the wallet.
///
/// The list is the whole truth for `addresses`: a reserved output of theirs
/// missing from it is treated as spent and released.
pub trait UtxoSource: Send + Sync {
    /// Every spendable output of `addresses`.
    ///
    /// # Errors
    ///
    /// Whatever the source cannot read; the send fails before any chain call.
    fn utxos<'a>(&'a self, addresses: &'a [Address]) -> FutureResult<'a, Vec<Utxo>>;
}

/// The caller selected the outputs: no store, no indexer. Returned as-is.
impl UtxoSource for Vec<Utxo> {
    fn utxos<'a>(&'a self, _addresses: &'a [Address]) -> FutureResult<'a, Vec<Utxo>> {
        Box::pin(async move { Ok(self.clone()) })
    }
}

/// The same list, shareable.
impl UtxoSource for Arc<Vec<Utxo>> {
    fn utxos<'a>(&'a self, addresses: &'a [Address]) -> FutureResult<'a, Vec<Utxo>> {
        (**self).utxos(addresses)
    }
}
