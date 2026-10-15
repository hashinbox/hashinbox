//! [`Estimator`]: what sending costs on one network, priced from cached
//! network state.

use crate::{Address, Asset, Decimal, Future, Networked};

/// Prices sends on one network from cached network state, so estimating costs
/// arithmetic plus, at most, one store read; RPC only on refresh.
///
/// A chain's wallet factory hands the same estimator to every builder it
/// makes, and builders price their transactions only through it, so an
/// estimate and the fee a builder pays agree.
pub trait Estimator: Networked + Send + Sync {
    /// Worst-case native fee of sending everything `sources` hold, each in its
    /// asset, to one new address: one transaction per sender on account chains,
    /// one transaction for all senders (same asset) on UTXO chains. A send funded
    /// by this amount goes through; it may overpay slightly, never underpay.
    ///
    /// # Errors
    ///
    /// An asset the network does not send, an address of another network,
    /// or a node or store failure while refreshing.
    fn estimate<'a>(
        &'a self,
        sources: &'a [(Address, Asset)],
    ) -> Future<'a, anyhow::Result<Decimal>>;
}
