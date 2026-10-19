//! [`Decimals`]: a network's asset decimals.

use std::collections::BTreeMap;

use anyhow::Context as _;
use base::Decimal;

/// Decimals per asset id of one network.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Decimals(pub(crate) BTreeMap<String, u32>);

impl Decimals {
    /// `value` in display units of `asset` as atomic units; it must be exact.
    ///
    /// # Errors
    ///
    /// When `asset` has no known decimals, or `value` is negative or has
    /// more digits than `asset`.
    pub(crate) fn atomic(&self, value: &Decimal, asset: &str) -> anyhow::Result<Decimal> {
        let decimals = *self
            .0
            .get(asset)
            .with_context(|| format!("asset {asset} has no known decimals"))?;
        Ok(Decimal::from_atomic(value.to_atomic(decimals)?, 0))
    }
}
