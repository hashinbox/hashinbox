//! What an asset's amounts are counted in, and where that is resolved.
//!
//! An index deals in atomic units and never learns an asset's decimals
//! (`docs/design/021` section 5.2), so an amount is scaled once, at the edge
//! that reports it. The scale belongs to the asset, not to a wallet: a chain
//! declares it for its own currency and an ERC-20 or SPL token declares its
//! own, checked against the contract when the chain connects. [`Units`] is
//! that declaration, keyed by [`AssetId`], as a composition root folds it.

use std::collections::{BTreeMap, btree_map};

use crate::{Asset, AssetId, Decimal, DecimalError};

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

/// Every configured asset's unit, by identity.
///
/// One value answers "how much is this amount" for every asset a process was
/// configured with, whichever chain declared it, so an edge that holds an
/// [`AssetId`] — one on every movement, every fee, and every balance — needs
/// nothing else to report it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Units(BTreeMap<AssetId, Unit>);

impl Units {
    /// The unit of `asset`, when a configured chain declared it.
    #[must_use]
    pub fn of(&self, asset: &AssetId) -> Option<&Unit> {
        self.0.get(asset)
    }
}

impl FromIterator<(AssetId, Unit)> for Units {
    fn from_iter<T: IntoIterator<Item = (AssetId, Unit)>>(declared: T) -> Self {
        Self(declared.into_iter().collect())
    }
}

impl Extend<(AssetId, Unit)> for Units {
    fn extend<T: IntoIterator<Item = (AssetId, Unit)>>(&mut self, declared: T) {
        self.0.extend(declared);
    }
}

impl IntoIterator for Units {
    type Item = (AssetId, Unit);
    type IntoIter = btree_map::IntoIter<AssetId, Unit>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChainName, ChainNetwork, NetworkKind};

    /// The example chain's 8-decimal currency.
    const COIN: Asset = Asset::new(
        ChainNetwork::new(
            ChainName::from_static("example"),
            "example",
            NetworkKind::Mainnet,
            "EXM",
        ),
        "Examplecoin",
        "EXM",
        8,
    );

    fn token() -> AssetId {
        AssetId {
            chain: ChainName::new("example"),
            asset: "token".to_owned(),
        }
    }

    #[test]
    fn a_currency_is_named_and_a_token_is_only_scaled() {
        let native = Unit::native(&COIN);

        assert_eq!(native.decimals, 8);
        assert_eq!(native.name.as_deref(), Some("Examplecoin"));
        assert_eq!(native.ticker.as_deref(), Some("EXM"));
        let token = Unit::token(6);
        assert_eq!(token.decimals, 6);
        assert_eq!((token.name, token.ticker), (None, None));
    }

    #[test]
    fn an_amount_is_scaled_by_its_own_assets_precision() {
        let units = Units::from_iter([
            (
                AssetId::native(ChainName::new("example")),
                Unit::native(&COIN),
            ),
            (token(), Unit::token(6)),
        ]);

        let coin = units
            .of(&AssetId::native(ChainName::new("example")))
            .expect("the declared currency")
            .display(&Decimal::from(123_000_000_u64))
            .expect("an exact amount");
        let token = units
            .of(&token())
            .expect("the declared token")
            .display(&Decimal::from(1_500_000_u64))
            .expect("an exact amount");

        assert_eq!(coin.to_string(), "1.23");
        assert_eq!(token.to_string(), "1.5");
    }

    #[test]
    fn an_undeclared_asset_has_no_unit_and_a_fractional_amount_is_refused() {
        let mut units = Units::default();
        units.extend([(token(), Unit::token(6))]);
        let fractional: Decimal = "0.1".parse().expect("decimal");

        assert_eq!(units.of(&AssetId::native(ChainName::new("example"))), None);
        let error = units
            .of(&token())
            .expect("the declared token")
            .display(&fractional)
            .expect_err("a fraction is not an atomic amount");
        assert_eq!(
            error.to_string(),
            "amount has more than 0 fractional digits"
        );
        assert_eq!(
            Units::from_iter(units).of(&token()),
            Some(&Unit::token(6)),
            "the declarations survive being folded into another map"
        );
    }
}
