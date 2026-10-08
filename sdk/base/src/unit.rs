//! What an asset's amounts are counted in, and where that is resolved.
//!
//! An index deals in atomic units and never learns an asset's decimals
//! so an amount is scaled once, at the edge
//! that reports it. The scale belongs to the asset, not to a wallet: a chain
//! declares it for its own currency and an ERC-20 or SPL token declares its
//! own, checked against the contract when the chain connects. [`Units`] is
//! that declaration, keyed by network and [`AssetId`], as a composition root folds it.

use std::collections::{BTreeMap, btree_map};

use crate::{Asset, AssetId, Decimal, DecimalError, Network};

/// The unit one asset's amounts are counted in: how many digits separate an
/// atomic amount from a display one, and what a caller calls it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// Fractional digits between the atomic unit and the display unit.
    pub decimals: u32,
    /// Display name, when the chain declares one.
    pub name: Option<String>,
    /// Display symbol, when the chain declares one.
    pub ticker: Option<String>,
}

impl Unit {
    /// The unit of a chain's own currency, from the metadata that chain
    /// declares for it ([`Asset`]).
    #[must_use]
    pub fn native(metadata: &Asset) -> Self {
        Self {
            decimals: metadata.decimals,
            name: Some(metadata.name.to_owned()),
            ticker: Some(metadata.ticker.to_owned()),
        }
    }

    /// The unit of a token, which names itself by its contract alone: the
    /// chain publishes no name or ticker for it, only the scale its contract
    /// reports.
    #[must_use]
    pub const fn token(decimals: u32) -> Self {
        Self {
            decimals,
            name: None,
            ticker: None,
        }
    }

    /// `atomic` — an exact, non-negative whole number of atomic units — in
    /// this unit.
    ///
    /// # Errors
    ///
    /// [`DecimalError`] when `atomic` is negative or fractional, which no
    /// indexed amount is.
    pub fn display(&self, atomic: &Decimal) -> Result<Decimal, DecimalError> {
        Ok(Decimal::from_atomic(atomic.to_atomic(0)?, self.decimals))
    }
}

/// Every configured asset's unit, by network and asset identity.
///
/// One value answers "how much is this amount" for every asset a process was
/// configured with, whichever chain declared it, so an edge that holds an
/// [`AssetId`] and its containing address or transaction network can report it.
/// Identical token text on different networks may declare different decimals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Units(BTreeMap<(Network, AssetId), Unit>);

impl Units {
    /// The unit of `asset` on exactly `network`, when that pair was declared.
    #[must_use]
    pub fn of(&self, network: &Network, asset: &AssetId) -> Option<&Unit> {
        self.0.get(&(network.clone(), asset.clone()))
    }
}

impl FromIterator<((Network, AssetId), Unit)> for Units {
    fn from_iter<T: IntoIterator<Item = ((Network, AssetId), Unit)>>(declared: T) -> Self {
        Self(declared.into_iter().collect())
    }
}

impl Extend<((Network, AssetId), Unit)> for Units {
    fn extend<T: IntoIterator<Item = ((Network, AssetId), Unit)>>(&mut self, declared: T) {
        self.0.extend(declared);
    }
}

impl IntoIterator for Units {
    type Item = ((Network, AssetId), Unit);
    type IntoIter = btree_map::IntoIter<(Network, AssetId), Unit>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
