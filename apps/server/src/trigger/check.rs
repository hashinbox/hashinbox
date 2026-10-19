//! Check and Filter: whether a trigger's [`Logic`] holds over `This`. Each
//! leaf decides itself from [`Reads`]; an unknown term fails the leaf, a read
//! error stays an error.

use anyhow::{Context as _, ensure};
use base::Decimal;

use super::{Aggregate, Amount, Compare, Filter, Gate, Logic, Reads};
use crate::models::wallet_index_balance::WalletIndexBalances;

/// A leaf of a check or a filter.
pub(crate) trait Leaf {
    /// Whether it holds over `this`.
    ///
    /// # Errors
    ///
    /// A read error (SQL, fee, decoding): never `false`.
    fn holds(&self, reads: &mut Reads<'_>, this: &WalletIndexBalances) -> anyhow::Result<bool>;
}

impl<L: Leaf> Logic<L> {
    /// Whether it holds over `this`, leaves in written order, stopping early.
    ///
    /// # Errors
    ///
    /// The first read error.
    pub(crate) fn holds(
        &self,
        reads: &mut Reads<'_>,
        this: &WalletIndexBalances,
    ) -> anyhow::Result<bool> {
        self.eval(&mut |leaf| leaf.holds(reads, this))
    }
}

impl Leaf for Gate {
    fn holds(&self, reads: &mut Reads<'_>, this: &WalletIndexBalances) -> anyhow::Result<bool> {
        let Some(run) = (match self {
            Self::Compare(compare) => return compare.holds(reads, this),
            Self::Idle | Self::Since { .. } => reads.run()?,
        }) else {
            return Ok(true);
        };
        Ok(match self {
            Self::Since { seconds } => {
                let wait = i64::try_from(*seconds)
                    .ok()
                    .and_then(|seconds| seconds.checked_mul(1000))
                    .context("a wait is out of range")?;
                reads.now.saturating_sub(run.started_at) >= wait
            }
            _ => run.settled,
        })
    }
}

impl Leaf for Filter {
    fn holds(&self, reads: &mut Reads<'_>, this: &WalletIndexBalances) -> anyhow::Result<bool> {
        let Self::Compare(compare) = self;
        compare.holds(reads, this)
    }
}

impl Leaf for Compare {
    fn holds(&self, reads: &mut Reads<'_>, this: &WalletIndexBalances) -> anyhow::Result<bool> {
        let Some(indexes) = reads.target(&self.of, this)? else {
            return Ok(false);
        };
        if self.aggregate == Aggregate::Count {
            // A count compares with a plain whole number.
            let Amount::Value(value) = &self.value else {
                anyhow::bail!("a count compares with a value");
            };
            ensure!(value.scale() == 0, "a count compares with a whole number");
            let count = Decimal::from(u64::try_from(indexes.len())?);
            return Ok(self.operator.holds(&count, value));
        }
        let asset = self
            .of
            .asset()
            .map(str::to_owned)
            .or_else(|| this.values().next().map(|one| one.index.asset.id.clone()));
        let Some(asset) = asset else {
            return Ok(false);
        };
        let balances = indexes.confirmed();
        let left = match self.aggregate {
            Aggregate::Min => balances.min(),
            Aggregate::Max => balances.max(),
            _ => Some(balances.sum()),
        };
        let (Some(left), Some(right)) = (left, self.value.eval(reads, this, &asset)?) else {
            return Ok(false);
        };
        Ok(self.operator.holds(&left, &right))
    }
}

impl Amount {
    /// In atomic units of `asset`; `None` when a term is unknown.
    fn eval(
        &self,
        reads: &mut Reads<'_>,
        this: &WalletIndexBalances,
        asset: &str,
    ) -> anyhow::Result<Option<Decimal>> {
        let (a, b, combine): (_, _, fn(Decimal, Decimal) -> Decimal) = match self {
            Self::Value(value) => return reads.decimals.atomic(value, asset).map(Some),
            Self::Balance { of } => {
                let Some(indexes) = reads.target(of, this)? else {
                    return Ok(None);
                };
                return Ok(Some(indexes.confirmed().sum()));
            }
            Self::Fee { of } => {
                let Some(indexes) = reads.target(of, this)? else {
                    return Ok(None);
                };
                return reads.fee(&indexes).map(Some);
            }
            Self::Add(a, b) => (a, b, |a, b| a + b),
            Self::Sub(a, b) => (a, b, |a, b| a - b),
            Self::Min(a, b) => (a, b, Decimal::min),
            Self::Max(a, b) => (a, b, Decimal::max),
        };
        let a = a.eval(reads, this, asset)?;
        let b = b.eval(reads, this, asset)?;
        Ok(a.zip(b).map(|(a, b)| combine(a, b)))
    }
}
