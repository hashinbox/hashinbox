//! A trigger evaluated over its network: [`Triggers`], a network's enabled
//! triggers read at its confirmed block (what a reorg cannot undo); each [`Trigger`] selects its wallet
//! indexes, keeps those its filter keeps and tells whether its check holds.
//! It ends at the decision ([`Decision`]): what the action does with it is
//! the caller's (the triggers worker).
//!
//! A leaf that refers to something unknown (an index not fully indexed, an
//! asset without balances) is `false`; a read error stays an error.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context as _, bail, ensure};
use base::{Assets, Decimal, FeeEstimator, Network, Networked as _};
use diesel::SqliteConnection;

use crate::models::{
    self, TriggerRun, network_index, trigger,
    trigger::{Action, Aggregate, Amount, Compare, Filter, Gate, Logic, Stats, Target},
    wallet_index::{self, Selection},
};

/// What every trigger of a round decides with, besides the connection.
#[derive(Clone)]
pub(crate) struct Context {
    /// The network's assets.
    pub(crate) assets: Assets,
    pub(crate) estimator: Arc<dyn FeeEstimator>,
    /// Where a fee estimate is awaited (decisions run on blocking threads).
    pub(crate) runtime: tokio::runtime::Handle,
    /// Unix ms.
    pub(crate) now: i64,
}

/// One enabled trigger deciding: its row, what it reads, and what it
/// resolved so far, so its filter, check and action see the same figures.
pub(crate) struct Trigger<'a> {
    row: models::Trigger,
    connection: &'a mut SqliteConnection,
    context: Context,
    /// Its latest run.
    run: Option<TriggerRun>,
    /// What targets named, by (`This` ids, target JSON); `None`: not fully
    /// indexed.
    targets: BTreeMap<(Vec<String>, String), Option<Selection>>,
}

impl<'a> Trigger<'a> {
    /// `row` deciding over `connection` with `context`, its latest run `run`.
    pub(crate) fn new(
        row: models::Trigger,
        connection: &'a mut SqliteConnection,
        context: Context,
        run: Option<TriggerRun>,
    ) -> Self {
        Self {
            row,
            connection,
            context,
            run,
            targets: BTreeMap::new(),
        }
    }

    /// What it decides on `network` at its confirmed position `confirmed`:
    /// the wallet indexes its selector picks, with spendable balances, and
    /// those its filter keeps (`This`) if its check holds; `None` when it
    /// does not. `stats` counts each step as it is taken, so a refusal keeps
    /// what was counted before it.
    ///
    /// # Errors
    ///
    /// When a read or the fee estimate fails.
    fn decide(
        &mut self,
        network: &Network,
        confirmed: i64,
        stats: &mut Stats,
    ) -> anyhow::Result<Option<Selection>> {
        let row = self.row.clone();
        let selected =
            wallet_index::db::select(self.connection, network, &row.selector.0, confirmed)?;
        stats.selected = selected.len();
        let kept = match &row.filter {
            Some(filter) => self.filter(&filter.0, &selected)?,
            None => selected,
        };
        stats.kept = kept.len();
        stats.checked = self.check(&row.check.0, &kept)?;
        Ok(stats.checked.then_some(kept))
    }

    /// Whether `check` holds over `this`, leaves in written order, stopping early.
    ///
    /// # Errors
    ///
    /// The first read error.
    pub(crate) fn check(&mut self, check: &Logic<Gate>, this: &Selection) -> anyhow::Result<bool> {
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
        this: &Selection,
    ) -> anyhow::Result<Selection> {
        let mut kept = Selection::new(this.network().clone(), this.confirmed_at());
        for one in this.values() {
            let alone = this.only(one)?;
            if filter.eval(&mut |Filter::Compare(compare)| self.compare(compare, &alone))? {
                kept.insert(one.clone())?;
            }
        }
        Ok(kept)
    }

    /// A check leaf: a compare, or a test of the latest run (none: passes).
    fn gate(&mut self, gate: &Gate, this: &Selection) -> anyhow::Result<bool> {
        Ok(match (gate, &self.run) {
            (Gate::Compare(compare), _) => self.compare(compare, this)?,
            (_, None) => true,
            (Gate::Since { seconds }, Some(run)) => {
                let wait = i64::try_from(*seconds)
                    .ok()
                    .and_then(|seconds| seconds.checked_mul(1000))
                    .context("a wait is out of range")?;
                self.context.now.saturating_sub(run.started_at) >= wait
            }
        })
    }

    /// Whether the aggregate of what `compare` refers to compares as it says.
    fn compare(&mut self, compare: &Compare, this: &Selection) -> anyhow::Result<bool> {
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
            .or_else(|| this.values().next().map(|one| one.asset.id.clone()));
        let Some(asset) = asset else {
            return Ok(false);
        };
        let balances = indexes
            .values()
            .map(|one| one.held(compare.balance))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter();
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
    pub(crate) fn amount(
        &mut self,
        amount: &Amount,
        this: &Selection,
        asset: &str,
    ) -> anyhow::Result<Option<Decimal>> {
        let (a, b, combine): (_, _, fn(Decimal, Decimal) -> Decimal) = match amount {
            Amount::Value(value) => {
                let id = base::Asset {
                    network: this.network().clone(),
                    id: asset.to_owned(),
                };
                let info = self
                    .context
                    .assets
                    .get(&id)
                    .with_context(|| format!("asset {asset} is not configured"))?;
                return Ok(Some(info.atomic(value)?));
            }
            Amount::Balance { of, balance } => {
                let indexes = self.target(of, this)?;
                return indexes
                    .map(|indexes| indexes.values().map(|one| one.held(*balance)).sum())
                    .transpose();
            }
            Amount::Fee { of, to } => {
                let Some(indexes) = self.target(of, this)? else {
                    return Ok(None);
                };
                // One recipient address is priced as it is; none named, or
                // several, as a new address.
                let to = match to {
                    Some(to) => {
                        let Some(to) = self.target(to, this)? else {
                            return Ok(None);
                        };
                        to.address().cloned()
                    }
                    None => None,
                };
                // Indexes that hold nothing send nothing: no fee, nothing asked.
                let spends = indexes.spends(to.as_ref())?;
                if spends.is_empty() {
                    return Ok(Some(Decimal::zero()));
                }
                let fee = self
                    .context
                    .runtime
                    .block_on(self.context.estimator.estimate(&spends))?;
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
    pub(crate) fn target(
        &mut self,
        target: &Target,
        this: &Selection,
    ) -> anyhow::Result<Option<Selection>> {
        let key = (this.ids(), serde_json::to_string(target)?);
        if let Some(found) = self.targets.get(&key) {
            return Ok(found.clone());
        }
        let found = wallet_index::db::find_by_target(self.connection, this, target)?;
        self.targets.insert(key, found.clone());
        Ok(found)
    }
}

/// Every enabled trigger of one network, at its confirmed block.
pub(crate) struct Triggers {
    network: Network,
    /// The confirmed position; `None` while unknown.
    confirmed: Option<i64>,
    /// Each with its latest run.
    all: Vec<(models::Trigger, Option<TriggerRun>)>,
}

impl Triggers {
    /// Reads them; none while `network`'s confirmed position is unknown.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub(crate) fn new(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Self> {
        let confirmed = network_index::db::find_confirmed(connection, network)?;
        let all = match confirmed {
            Some(_) => trigger::db::find_enabled(connection, network)?,
            None => Vec::new(),
        };
        Ok(Self {
            network: network.clone(),
            confirmed,
            all,
        })
    }

    /// The confirmed position they decide at; `None` while unknown.
    pub(crate) const fn confirmed(&self) -> Option<i64> {
        self.confirmed
    }

    /// Each one's id, what it saw, and what `act` made of its decision, in
    /// id order: `None` when its check does not hold, the error when it
    /// cannot decide or `act` fails.
    pub(crate) fn decide<T>(
        &self,
        connection: &mut SqliteConnection,
        context: &Context,
        mut act: impl FnMut(Decision<'_, '_>) -> anyhow::Result<T>,
    ) -> Vec<(String, Stats, anyhow::Result<Option<T>>)> {
        let Some(confirmed) = self.confirmed else {
            return Vec::new();
        };
        let mut decided = Vec::with_capacity(self.all.len());
        for (row, run) in &self.all {
            let mut trigger = Trigger::new(row.clone(), connection, context.clone(), run.clone());
            let mut stats = Stats::default();
            let chosen = trigger.decide(&self.network, confirmed, &mut stats);
            let outcome = chosen.and_then(|this| {
                let Some(this) = this else {
                    return Ok(None);
                };
                act(Decision {
                    action: &row.action.0,
                    network: &self.network,
                    this: &this,
                    trigger: &mut trigger,
                })
                .map(Some)
            });
            decided.push((row.id.clone(), stats, outcome));
        }
        decided
    }
}

/// A trigger whose check holds: what its action runs with.
pub(crate) struct Decision<'a, 'c> {
    /// The action it names, with its static input.
    pub(crate) action: &'a Action,
    /// The trigger's network.
    pub(crate) network: &'a Network,
    /// The wallet indexes its filter kept (`This`), with balances.
    pub(crate) this: &'a Selection,
    /// The trigger still deciding: amounts and targets read at the same block.
    pub(crate) trigger: &'a mut Trigger<'c>,
}
