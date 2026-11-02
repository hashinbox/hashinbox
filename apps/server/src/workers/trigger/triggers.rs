//! [`Triggers`]: a network's enabled triggers, read at its last kept block;
//! each [`Trigger`] decides what its action starts.
//!
//! A leaf that refers to something unknown (an index not fully indexed, an
//! asset without balances) is `false`; a read error stays an error.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use anyhow::{Context as _, bail, ensure};
use base::{Address, Assets, Decimal, FeeEstimator, Network, Networked as _};
use diesel::SqliteConnection;
use tasks::Job;

use super::{Actions, Invocation};
use crate::models::{
    self, TriggerRun, block, trigger,
    trigger::{Aggregate, Amount, Compare, Filter, Gate, Logic, Target},
    trigger_run, wallet,
    wallet_index_balance::{self, WalletIndexBalances},
};

/// What every trigger of a round decides with, besides the connection.
#[derive(Clone)]
pub(crate) struct Context {
    /// The network's assets.
    pub(crate) assets: Assets,
    pub(crate) estimator: Arc<dyn FeeEstimator>,
    /// Where a fee estimate is awaited (decisions run on blocking threads).
    pub(crate) runtime: tokio::runtime::Handle,
    /// What a trigger's action names.
    pub(crate) actions: Arc<Actions>,
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
    targets: BTreeMap<(Vec<String>, String), Option<WalletIndexBalances>>,
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

    /// What it starts on `network` at block position `last`: the wallet
    /// indexes its selector picks, with spendable balances; those its filter
    /// keeps (`This`); if its check holds, what its action starts. Empty:
    /// nothing to do.
    ///
    /// # Errors
    ///
    /// When a read, the fee estimate or the action fails.
    fn decide(
        &mut self,
        network: &Network,
        last: i64,
        keyed: &BTreeSet<Address>,
    ) -> anyhow::Result<Vec<Job>> {
        let row = self.row.clone();
        let selected =
            wallet_index_balance::db::select(self.connection, network, &row.selector.0, last)?;
        let kept = match &row.filter {
            Some(filter) => self.filter(&filter.0, &selected)?,
            None => selected,
        };
        if !self.check(&row.check.0, &kept)? {
            return Ok(Vec::new());
        }
        let actions = self.context.actions.clone();
        let mut invocation = Invocation::new(network, &kept, self, keyed);
        actions.run(&row.action.0.name, &row.action.0.input, &mut invocation)?;
        Ok(invocation.started())
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
        let balances = indexes.spendable();
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
                    .context
                    .assets
                    .get(&id)
                    .with_context(|| format!("asset {asset} is not configured"))?;
                return Ok(Some(info.atomic(value)?));
            }
            Amount::Balance { of } => {
                let indexes = self.target(of, this)?;
                return Ok(indexes.map(|indexes| indexes.spendable().sum()));
            }
            Amount::Fee { of } => {
                let Some(indexes) = self.target(of, this)? else {
                    return Ok(None);
                };
                if indexes.is_empty() {
                    return Ok(Some(Decimal::zero()));
                }
                let fee = self
                    .context
                    .runtime
                    .block_on(self.context.estimator.estimate(&indexes.sources()))?;
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

/// Every enabled trigger of one network, at its last kept block, with the
/// addresses this server holds keys for.
pub(crate) struct Triggers {
    network: Network,
    /// The last kept block's position; `None` before the first block.
    last: Option<i64>,
    keyed: BTreeSet<Address>,
    all: Vec<models::Trigger>,
}

impl Triggers {
    /// Reads them; none before `network`'s first kept block.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub(crate) fn new(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Self> {
        let last = block::db::find_last(connection, network)?.map(|block| block.position);
        let all = match last {
            Some(_) => trigger::db::find_enabled(connection, network)?
                .iter()
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        let keyed = wallet::db::find_all(connection)?
            .addresses()
            .into_iter()
            .collect();
        Ok(Self {
            network: network.clone(),
            last,
            keyed,
            all,
        })
    }

    /// Each one's id and what it starts, or why it cannot, in id order.
    pub(crate) fn decide(
        &self,
        connection: &mut SqliteConnection,
        context: &Context,
    ) -> Vec<(String, anyhow::Result<Vec<Job>>)> {
        let Some(last) = self.last else {
            return Vec::new();
        };
        self.all
            .iter()
            .map(|row| {
                let jobs = trigger_run::db::find_last(connection, &row.id)
                    .map_err(anyhow::Error::from)
                    .and_then(|run| {
                        Trigger::new(row.clone(), connection, context.clone(), run).decide(
                            &self.network,
                            last,
                            &self.keyed,
                        )
                    });
                (row.id.clone(), jobs)
            })
            .collect()
    }
}
