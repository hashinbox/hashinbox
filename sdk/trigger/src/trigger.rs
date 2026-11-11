//! One trigger: its [`Trigger`] definition and rules, the [`Context`] it
//! decides in, and how it decides: select, keep each index by its filter,
//! check over the kept set, call its action.

use std::collections::BTreeSet;

use base::{Address, Asset, Assets, Decimal, FeeEstimator, Network};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    actions::{Call, Registry},
    conditions::{Condition, Logic},
    error::{Class, Error},
    round::Stats,
    selection::{Selection, Selector, Target, This},
    store::Store,
};

/// The state of one trigger deciding: the wallets being evaluated (`this`),
/// the round's reads, and the trigger's last run.
pub struct Context<'r> {
    store: &'r mut dyn Store,
    fees: &'r dyn FeeEstimator,
    assets: &'r Assets,
    network: &'r Network,
    now: u64,
    last_run: Option<u64>,
    this: Selection,
}

impl<'r> Context<'r> {
    /// A context on `network` reading `store`, pricing with `fees`, at
    /// `now` (Unix ms), the trigger last run at `last_run`; `this` is empty
    /// until [`Context::with`] sets it.
    #[must_use]
    pub fn new(
        store: &'r mut dyn Store,
        fees: &'r dyn FeeEstimator,
        assets: &'r Assets,
        network: &'r Network,
        now: u64,
        last_run: Option<u64>,
    ) -> Self {
        Self {
            store,
            fees,
            assets,
            network,
            now,
            last_run,
            this: Selection::new(network.clone()),
        }
    }

    /// The wallets being evaluated: `this`.
    #[must_use]
    pub const fn this(&self) -> &Selection {
        &self.this
    }

    /// The same context with `this` replaced: a filter per index, an
    /// amount per source.
    pub fn with(&mut self, this: Selection) -> Context<'_> {
        Context {
            store: &mut *self.store,
            fees: self.fees,
            assets: self.assets,
            network: self.network,
            now: self.now,
            last_run: self.last_run,
            this,
        }
    }

    /// The wallets `target` names; `None` when a sibling `This {asset}`
    /// names is not fully indexed.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub async fn wallets(&mut self, target: &Target) -> anyhow::Result<Option<Selection>> {
        match target {
            Target::This(This { asset: None }) => Ok(Some(self.this.clone())),
            Target::This(This { asset: Some(asset) }) => self.siblings(asset).await,
            Target::Select(selector) => self.select(selector).await.map(Some),
        }
    }

    /// The fully indexed indexes `selector` picks.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub async fn select(&mut self, selector: &Selector) -> anyhow::Result<Selection> {
        let found = self.store.select(selector).await?;
        Selection::of(self.network.clone(), found)
    }

    /// The `asset` index of each address of `this`, once; `None` when one
    /// is not fully indexed.
    async fn siblings(&mut self, asset: &str) -> anyhow::Result<Option<Selection>> {
        let asset = Asset {
            network: self.network.clone(),
            id: asset.to_owned(),
        };
        let wanted: BTreeSet<(Address, Asset)> = self
            .this
            .values()
            .map(|one| (one.address.clone(), asset.clone()))
            .collect();
        let wanted: Vec<_> = wanted.into_iter().collect();
        let found = self.store.find(&wanted).await?;
        let found = Selection::of(self.network.clone(), found)?;
        Ok((found.len() == wanted.len()).then_some(found))
    }

    /// The chain's fee estimator.
    #[must_use]
    pub fn fees(&self) -> &dyn FeeEstimator {
        self.fees
    }

    /// `value` in display units of `asset`, in atomic units.
    ///
    /// # Errors
    ///
    /// When `asset` is not configured, or `value` is negative or has more
    /// decimals than it.
    pub fn atomic(&self, asset: &Asset, value: &Decimal) -> anyhow::Result<Decimal> {
        let Some(info) = self.assets.get(asset) else {
            anyhow::bail!("asset {} is not configured", asset.id);
        };
        Ok(info.atomic(value)?)
    }

    /// The network it decides on.
    #[must_use]
    pub const fn network(&self) -> &Network {
        self.network
    }

    /// Unix ms.
    #[must_use]
    pub const fn now(&self) -> u64 {
        self.now
    }

    /// When this trigger's latest run started, Unix ms; `None`: never.
    #[must_use]
    pub const fn last_run(&self) -> Option<u64> {
        self.last_run
    }
}

/// A trigger: which wallets (`selector`), which of them each on its own
/// (`filter`), when over those kept (`check`), and what it does (`action`).
/// `F` and `C` are the caller's trigger language.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(deserialize = "F: DeserializeOwned, C: DeserializeOwned"))]
pub struct Trigger<F, C> {
    /// Which wallet indexes it acts on.
    pub selector: Selector,
    /// Which of them it keeps, each tested alone as `this`; `None`: all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Logic<F>>,
    /// When it acts, over the kept ones.
    pub check: Logic<C>,
    /// What it does.
    pub action: Call,
}

impl<F: Condition, C: Condition> Trigger<F, C> {
    /// Whether it may be stored: an action `actions` accepts, wallets named
    /// on purpose, a filter whose every condition reads the wallet it tests,
    /// and a filter and check that can hold.
    ///
    /// # Errors
    ///
    /// The first rule it breaks.
    pub fn validate<E: 'static>(
        &self,
        actions: &Registry<E>,
        network: &Network,
        assets: &Assets,
    ) -> Result<(), Error> {
        actions.validate(&self.action, network, assets)?;
        self.selector.validate()?;
        if let Some(filter) = &self.filter {
            filter.validate()?;
            if !filter.reads_this() {
                return Err(Error::new(
                    Class::FilterIgnoresWallet,
                    "a filter that never looks at this wallet keeps all or none; put it under When",
                ));
            }
        }
        self.check.validate()
    }

    /// What it does now over `on`: the indexes its selector picks, those its
    /// filter keeps, and its action's effects if its check holds over them.
    /// `stats` counts each step as it is taken, so a refusal keeps what was
    /// counted before it.
    ///
    /// # Errors
    ///
    /// When a read, a fee estimate or the action fails.
    pub async fn decide<E: 'static>(
        &self,
        actions: &Registry<E>,
        on: &mut Context<'_>,
        stats: &mut Stats,
    ) -> anyhow::Result<Vec<E>> {
        let selected = on.select(&self.selector).await?;
        stats.selected = selected.len();
        let kept = self.keep(on, selected).await?;
        stats.kept = kept.len();
        let mut on = on.with(kept);
        stats.checked = self.check.holds(&mut on).await?;
        if !stats.checked {
            return Ok(Vec::new());
        }
        actions.run(&self.action, &mut on).await
    }

    /// The indexes of `selected` its filter keeps, each tested alone.
    async fn keep(&self, on: &mut Context<'_>, selected: Selection) -> anyhow::Result<Selection> {
        let Some(filter) = &self.filter else {
            return Ok(selected);
        };
        let mut kept = Selection::new(selected.network().clone());
        for one in selected.values() {
            let alone = selected.only(one)?;
            if filter.holds(&mut on.with(alone)).await? {
                kept.insert(one.clone())?;
            }
        }
        Ok(kept)
    }
}
