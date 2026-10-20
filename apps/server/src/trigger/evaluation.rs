//! [`Evaluation`]: which indexes a trigger's filter keeps, and whether its
//! check holds over them.
//!
//! A leaf that refers to something unknown (an index not fully indexed, an
//! asset without balances) is `false`; a read error stays an error.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail, ensure};
use std::sync::Arc;

use base::{Assets, Decimal, FeeEstimator, Networked as _};
use diesel::SqliteConnection;

use crate::models::{
    TriggerRun,
    trigger::{Aggregate, Amount, Compare, Filter, Gate, Logic, Target},
    wallet_index_balance::{self, WalletIndexBalances},
};

/// One trigger evaluated at `now`: what it reads, and what it resolved so
/// far, so its filter and check see the same figures.
pub(crate) struct Evaluation<'a> {
    connection: &'a mut SqliteConnection,
    /// Unix ms.
    now: i64,
    /// The trigger's latest run.
    run: Option<TriggerRun>,
    /// The network's assets, for display amounts.
    assets: Assets,
    /// The network's fee estimator, awaited on `runtime`: an evaluation runs
    /// on a blocking thread.
    estimator: Arc<dyn FeeEstimator>,
    runtime: tokio::runtime::Handle,
    /// What targets named, by (`This` ids, target JSON); `None`: not fully
    /// indexed.
    targets: BTreeMap<(Vec<String>, String), Option<WalletIndexBalances>>,
}

impl<'a> Evaluation<'a> {
    /// The evaluation of a trigger whose latest run is `run`, at `now`.
    pub(crate) fn new(
        connection: &'a mut SqliteConnection,
        now: i64,
        run: Option<TriggerRun>,
        assets: Assets,
        estimator: Arc<dyn FeeEstimator>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            connection,
            now,
            run,
            assets,
            estimator,
            runtime,
            targets: BTreeMap::new(),
        }
    }

    /// Whether `check` holds over `this`, leaves in written order, stopping early.
    ///
    /// # Errors
    ///
    /// The first read error.
    pub(crate) fn check(
        &mut self,
        check: &Logic<Gate>,
        this: &WalletIndexBalances,
    ) -> anyhow::Result<bool> {
        check.eval(&mut |gate| self.gate(gate, this))
    }

    /// The indexes of `this` that `filter` keeps, each tested alone as
    /// `This`; an index whose filter refers to something unknown is dropped.
    ///
    /// # Errors
    ///
    /// The first read error.
    pub(crate) fn filter(
        &mut self,
        filter: &Logic<Filter>,
        this: &WalletIndexBalances,
    ) -> anyhow::Result<WalletIndexBalances> {
        let mut kept = this.emptied();
        for one in this.values() {
            let alone = this.only(one)?;
            if filter.eval(&mut |Filter::Compare(compare)| self.compare(compare, &alone))? {
                kept.insert(one.clone())?;
            }
        }
        Ok(kept)
    }

    /// A check leaf: a compare, or a test of the latest run (none: passes).
    fn gate(&mut self, gate: &Gate, this: &WalletIndexBalances) -> anyhow::Result<bool> {
        Ok(match (gate, &self.run) {
            (Gate::Compare(compare), _) => self.compare(compare, this)?,
            (_, None) => true,
            (Gate::Idle, Some(run)) => run.settled_at.is_some(),
            (Gate::Since { seconds }, Some(run)) => {
                let wait = i64::try_from(*seconds)
                    .ok()
                    .and_then(|seconds| seconds.checked_mul(1000))
                    .context("a wait is out of range")?;
                self.now.saturating_sub(run.started_at) >= wait
            }
        })
    }

    /// Whether the aggregate of what `compare` refers to compares as it says.
    fn compare(&mut self, compare: &Compare, this: &WalletIndexBalances) -> anyhow::Result<bool> {
        let Some(indexes) = self.target(&compare.of, this)? else {
            return Ok(false);
        };
        if compare.aggregate == Aggregate::Count {
            // A count compares with a plain whole number.
            let Amount::Value(value) = &compare.value else {
                bail!("a count compares with a value");
            };
            ensure!(value.scale() == 0, "a count compares with a whole number");
            let count = Decimal::from(u64::try_from(indexes.len())?);
            return Ok(compare.operator.holds(&count, value));
        }
        // Plain `This` is in the asset of the indexes in context.
        let asset = compare
            .of
            .asset()
            .map(str::to_owned)
            .or_else(|| this.values().next().map(|one| one.index.asset.id.clone()));
        let Some(asset) = asset else {
            return Ok(false);
        };
        let balances = indexes.confirmed();
        let left = match compare.aggregate {
            Aggregate::Min => balances.min(),
            Aggregate::Max => balances.max(),
            _ => Some(balances.sum()),
        };
        let right = self.amount(&compare.value, this, &asset)?;
        let (Some(left), Some(right)) = (left, right) else {
            return Ok(false);
        };
        Ok(compare.operator.holds(&left, &right))
    }

    /// `amount` in atomic units of `asset`; `None` when a term is unknown.
    pub(super) fn amount(
        &mut self,
        amount: &Amount,
        this: &WalletIndexBalances,
        asset: &str,
    ) -> anyhow::Result<Option<Decimal>> {
        let (a, b, combine): (_, _, fn(Decimal, Decimal) -> Decimal) = match amount {
            Amount::Value(value) => {
                let id = base::Asset {
                    network: this.network().clone(),
                    id: asset.to_owned(),
                };
                let info = self
                    .assets
                    .get(&id)
                    .with_context(|| format!("asset {asset} is not configured"))?;
                return Ok(Some(info.atomic(value)?));
            }
            Amount::Balance { of } => {
                let indexes = self.target(of, this)?;
                return Ok(indexes.map(|indexes| indexes.confirmed().sum()));
            }
            Amount::Fee { of } => {
                let Some(indexes) = self.target(of, this)? else {
                    return Ok(None);
                };
                if indexes.is_empty() {
                    return Ok(Some(Decimal::zero()));
                }
                let fee = self
                    .runtime
                    .block_on(self.estimator.estimate(&indexes.sources()))?;
                return Ok(Some(fee));
            }
            Amount::Add(a, b) => (a, b, |a, b| a + b),
            Amount::Sub(a, b) => (a, b, |a, b| a - b),
            Amount::Min(a, b) => (a, b, Decimal::min),
            Amount::Max(a, b) => (a, b, Decimal::max),
        };
        let a = self.amount(a, this, asset)?;
        let b = self.amount(b, this, asset)?;
        Ok(a.zip(b).map(|(a, b)| combine(a, b)))
    }

    /// What `target` names with `this` in context, resolved once.
    pub(super) fn target(
        &mut self,
        target: &Target,
        this: &WalletIndexBalances,
    ) -> anyhow::Result<Option<WalletIndexBalances>> {
        let key = (this.ids(), serde_json::to_string(target)?);
        if let Some(found) = self.targets.get(&key) {
            return Ok(found.clone());
        }
        let found = wallet_index_balance::db::find_by_target(self.connection, this, target)?;
        self.targets.insert(key, found.clone());
        Ok(found)
    }
}
