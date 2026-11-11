//! [`Estimator`]: what sending costs on one network, priced from cached
//! network state; [`Holdings`]: what the application knows of who holds what,
//! so a send to an address that already holds its asset is priced as such.

use crate::{Address, Asset, Decimal, Future, Networked};

/// Prices sends on one network from cached network state, so estimating costs
/// arithmetic plus, at most, one store read; RPC only on refresh.
///
/// A chain's wallet factory hands the same estimator to every builder it
/// makes, and builders price their transactions only through it, so an
/// estimate and the fee a builder pays agree.
pub trait Estimator: Networked + Send + Sync {
    /// Worst-case native fee of `spends`, each everything its sender holds of
    /// its asset sent to its recipient: one transaction per sender on account
    /// chains, one transaction for all senders (same asset) on UTXO chains. A
    /// recipient that [`Holdings`] say holds the asset already is priced as
    /// such (less where a first receipt creates an account or balance); one
    /// that does not, or none given, as a new address. A send funded by this
    /// amount goes through; it may overpay slightly, never underpay.
    ///
    /// # Errors
    ///
    /// An asset the network does not send, an address of another network,
    /// or a node or store failure while refreshing.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Decimal>>;
}

/// One send an [`Estimator`] prices: who sends, which asset, and to whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spend {
    /// The sender, who pays the fee.
    pub from: Address,
    /// What it sends.
    pub asset: Asset,
    /// The recipient; `None`: a new address, the dearest case.
    pub to: Option<Address>,
}

/// `(from, asset)`: to a new address.
impl From<(Address, Asset)> for Spend {
    fn from((from, asset): (Address, Asset)) -> Self {
        Self {
            from,
            asset,
            to: None,
        }
    }
}

/// `(from, asset, to)`.
impl From<(Address, Asset, Address)> for Spend {
    fn from((from, asset, to): (Address, Asset, Address)) -> Self {
        Self {
            from,
            asset,
            to: Some(to),
        }
    }
}

/// Spends priced together: one estimate, one [`Holdings`] read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spends(Vec<Spend>);

impl Spends {
    /// Adds `spend`: `(from, asset)` or `(from, asset, to)`.
    pub fn push(&mut self, spend: impl Into<Spend>) {
        self.0.push(spend.into());
    }

    /// Each spend's recipient with its asset, in order; spends to a new
    /// address are left out.
    pub fn recipients(&self) -> impl Iterator<Item = (&Address, &Asset)> {
        self.0
            .iter()
            .filter_map(|spend| spend.to.as_ref().map(|to| (to, &spend.asset)))
    }
}

impl std::ops::Deref for Spends {
    type Target = [Spend];

    fn deref(&self) -> &[Spend] {
        &self.0
    }
}

impl<S: Into<Spend>> FromIterator<S> for Spends {
    fn from_iter<I: IntoIterator<Item = S>>(spends: I) -> Self {
        Self(spends.into_iter().map(Into::into).collect())
    }
}

/// What the application knows of who holds what, from its own records: a
/// send to an address that holds its asset already creates nothing, which
/// costs less on chains where a first receipt creates the recipient's account
/// (Ethereum, Tron) or balance (an ERC-20's, a Solana token account). Read
/// once per estimate or signing.
pub trait Holdings: Send + Sync {
    /// For each of `spends`, in order, whether its recipient, a wallet whose
    /// key is kept, holds its asset now; `false` without a recipient or when
    /// it is not known to. One read for them all.
    ///
    /// # Errors
    ///
    /// A store failure.
    fn holds<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Vec<bool>>>;
}

/// [`Holdings`] that know of no holder: every send is priced as to a new
/// address.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHoldings;

impl Holdings for NoHoldings {
    fn holds<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Vec<bool>>> {
        Box::pin(async { Ok(vec![false; spends.len()]) })
    }
}
