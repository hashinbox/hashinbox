//! Unspent outputs of UTXO chains.

use crate::{Address, Asset, Decimal, Future, Transacted, TransactionId};

/// A UTXO: the `Received` leg that created it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Coin {
    /// The creating transaction.
    pub transaction: TransactionId,
    /// The leg's index in it (Bitcoin: the vout).
    pub index: u32,
}

impl Transacted for Coin {
    fn txid(&self) -> &TransactionId {
        &self.transaction
    }
}

/// An unspent output of an address (UTXO chains).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The coin.
    pub coin: Coin,
    /// Its owner.
    pub address: Address,
    /// Its asset.
    pub asset: Asset,
    /// Atomic amount.
    pub amount: Decimal,
    /// Height of its creating block.
    pub height: u64,
}

/// Where unspent outputs are read.
pub trait Store: Send + Sync {
    /// Every unspent output of `address`.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_unspent_for_address<'a>(
        &'a self,
        address: &'a Address,
    ) -> Future<'a, anyhow::Result<Vec<Output>>>;
}
