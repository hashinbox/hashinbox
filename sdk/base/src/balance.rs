//! [`Balances`]: what an address holds of an asset at a final block, read
//! from the chain itself.

use crate::{Address, Asset, Decimal, Future, Networked};

/// One asset of one address at a final block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holding {
    /// The final block it is read at.
    pub position: u64,
    /// Atomic amount.
    pub amount: Decimal,
}

/// A chain that can read a balance at a final block: optional, a
/// [`Service`](crate::Service) may lack it.
pub trait Balances: Networked + Send + Sync {
    /// The balance at a final block at or above `at`, with that block's
    /// position. `None`: the chain has not finalized `at` yet.
    ///
    /// # Errors
    ///
    /// When the node cannot answer, or `asset` is not one the chain reads.
    fn balance<'a>(
        &'a self,
        address: &'a Address,
        asset: &'a Asset,
        at: u64,
    ) -> Future<'a, anyhow::Result<Option<Holding>>>;
}
