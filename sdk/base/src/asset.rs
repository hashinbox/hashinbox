use std::collections::{BTreeMap, btree_map};

use crate::{Decimal, DecimalError, Network};

/// One asset on one network; the chain's own currency is [`Asset::native`].
///
/// The same token text on two networks is two assets.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Asset {
    /// The network the asset lives on.
    pub network: Network,
    /// Chain-owned asset text: `native`, or, e.g., a token contract address.
    pub id: String,
}

/// Asset text naming a chain's own currency.
const NATIVE: &str = "native";

impl Asset {
    /// The network's own currency (BTC, ETH, SOL).
    #[must_use]
    pub fn native(network: Network) -> Self {
        Self {
            network,
            id: NATIVE.to_owned(),
        }
    }

    /// Whether this names the network's own currency.
    #[must_use]
    pub fn is_native(&self) -> bool {
        self.id == NATIVE
    }
}

/// The one description of an asset: what its amounts are counted in and
/// what a caller calls it. A chain declares it for its own currency (with a
/// name and ticker) and for each token (by scale alone).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AssetInfo {
    /// The asset described.
    pub asset: Asset,
    /// Fractional digits between the display unit and the atomic unit.
    pub decimals: u32,
    /// Human-readable name, such as `Bitcoin`; `None` for a token.
    pub name: Option<String>,
    /// Display symbol, such as `BTC`; `None` for a token.
    pub ticker: Option<String>,
}

impl AssetInfo {
    /// A token, which names itself by its contract alone: only the scale its
    /// contract reports.
    #[must_use]
    pub const fn token(asset: Asset, decimals: u32) -> Self {
        Self {
            asset,
            decimals,
            name: None,
            ticker: None,
        }
    }

    /// `atomic` — an exact, non-negative whole number of atomic units — in
    /// display units.
    ///
    /// # Errors
    ///
    /// [`DecimalError`] when `atomic` is negative or fractional, which no
    /// indexed amount is.
    pub fn display(&self, atomic: &Decimal) -> Result<Decimal, DecimalError> {
        Ok(Decimal::from_atomic(atomic.to_atomic(0)?, self.decimals))
    }
}

/// Every configured asset's description, by asset identity: what an edge
/// holding an [`Asset`] scales an indexed amount by. Identical token text on
/// different networks is two assets and may declare different decimals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assets(BTreeMap<Asset, AssetInfo>);

impl Assets {
    /// The description of `asset`, when it was declared.
    #[must_use]
    pub fn get(&self, asset: &Asset) -> Option<&AssetInfo> {
        self.0.get(asset)
    }
}

impl FromIterator<AssetInfo> for Assets {
    fn from_iter<T: IntoIterator<Item = AssetInfo>>(declared: T) -> Self {
        Self(
            declared
                .into_iter()
                .map(|info| (info.asset.clone(), info))
                .collect(),
        )
    }
}

impl Extend<AssetInfo> for Assets {
    fn extend<T: IntoIterator<Item = AssetInfo>>(&mut self, declared: T) {
        self.0
            .extend(declared.into_iter().map(|info| (info.asset.clone(), info)));
    }
}

impl IntoIterator for Assets {
    type Item = AssetInfo;
    type IntoIter = btree_map::IntoValues<Asset, AssetInfo>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_values()
    }
}
