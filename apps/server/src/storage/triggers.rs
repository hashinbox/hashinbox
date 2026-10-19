//! The trigger routes' reads and writes on the `trigger`, `trigger_run` and
//! `tag` models, and a trigger's preview: what it selects now and whether
//! its filter and check hold.

use std::sync::Arc;

use anyhow::Context as _;
use base::{Address, FeeEstimator, Network};

use super::Storage;
use crate::{
    models::{
        Tag, Trigger, TriggerRun, block,
        tag::{self, Tags},
        trigger::{self, Filter, Gate, Logic, Triggers},
        trigger_run,
        wallet_index::Selector,
        wallet_index_balance::{self, WalletIndexBalances},
    },
    trigger::{Decimals, Evaluation, Fees},
};

/// What a trigger would see now.
pub(crate) struct Preview {
    /// The confirmed tip position read; `None` before the first block.
    pub(crate) confirmed: Option<i64>,
    /// What its selector picks, with balances.
    pub(crate) selected: WalletIndexBalances,
    /// Whether its filter holds; `None` without a filter.
    pub(crate) filter: Option<bool>,
    /// Whether its check holds.
    pub(crate) check: bool,
}

/// What a preview evaluates with.
pub(crate) struct PreviewSources {
    pub(crate) decimals: Decimals,
    pub(crate) estimator: Arc<dyn FeeEstimator>,
    /// Unix ms.
    pub(crate) now: i64,
}

impl Storage {
    /// Every trigger, by id.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) async fn triggers(&self) -> anyhow::Result<Triggers> {
        self.read(|connection| Ok(trigger::db::find_all(connection)?))
            .await
    }

    /// The trigger `id`, when kept.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) async fn trigger(&self, id: &str) -> anyhow::Result<Option<Trigger>> {
        let id = id.to_owned();
        self.read(move |connection| {
            use diesel::OptionalExtension as _;
            Ok(trigger::db::get_by_id(connection, &id).optional()?)
        })
        .await
    }

    /// Keeps a new `trigger`; `false` when its id is taken.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) async fn insert_trigger(&self, trigger: Trigger) -> anyhow::Result<bool> {
        self.write(move |connection| {
            use diesel::OptionalExtension as _;
            if trigger::db::get_by_id(connection, &trigger.id)
                .optional()?
                .is_some()
            {
                return Ok(false);
            }
            trigger::db::insert(connection, &trigger)?;
            Ok(true)
        })
        .await
    }

    /// Replaces kept trigger `trigger.id`; `false` when none is kept.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) async fn update_trigger(&self, trigger: Trigger) -> anyhow::Result<bool> {
        self.write(move |connection| Ok(trigger::db::update(connection, &trigger)? == 1))
            .await
    }

    /// Forgets trigger `id` and its runs; `false` when none is kept.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) async fn delete_trigger(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_owned();
        self.write(move |connection| Ok(trigger::db::delete(connection, &id)? == 1))
            .await
    }

    /// The latest `limit` runs of trigger `id`, latest first.
    ///
    /// # Errors
    ///
    /// When the read fails.
    pub(crate) async fn trigger_runs(
        &self,
        id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<TriggerRun>> {
        let id = id.to_owned();
        self.read(move |connection| Ok(trigger_run::db::find_recent(connection, &id, limit)?))
            .await
    }

    /// Every tag.
    ///
    /// # Errors
    ///
    /// When the read fails.
    pub(crate) async fn tags(&self) -> anyhow::Result<Tags> {
        self.read(|connection| Ok(tag::db::find_all(connection)?))
            .await
    }

    /// Tags `address` with `name`; tagging twice keeps one.
    ///
    /// # Errors
    ///
    /// When `name` is not `[a-z0-9-]{1,64}` or the write fails.
    pub(crate) async fn add_tag(&self, address: &Address, name: &str) -> anyhow::Result<()> {
        let tag = Tag::new(address, name)?;
        self.write(move |connection| {
            tag::db::insert(connection, &tag)?;
            Ok(())
        })
        .await
    }

    /// Removes tag `name` from `address`; `false` when it had none.
    ///
    /// # Errors
    ///
    /// When `name` is not `[a-z0-9-]{1,64}` or the write fails.
    pub(crate) async fn remove_tag(&self, address: &Address, name: &str) -> anyhow::Result<bool> {
        let id = Tag::id(address, name)?;
        self.write(move |connection| Ok(tag::db::delete(connection, &id)? == 1))
            .await
    }

    /// What a trigger on `network` with `selector`, `filter` and `check`
    /// sees at the last kept block; a never-run trigger.
    ///
    /// # Errors
    ///
    /// When a read or the fee estimate fails.
    pub(crate) async fn preview(
        &self,
        network: Network,
        selector: Selector,
        filter: Option<Logic<Filter>>,
        check: Logic<Gate>,
        sources: PreviewSources,
    ) -> anyhow::Result<Preview> {
        let runtime = tokio::runtime::Handle::current();
        self.read(move |connection| {
            let Some(last) = block::db::find_last(connection, &network)? else {
                return Ok(Preview {
                    confirmed: None,
                    selected: WalletIndexBalances::new(network, 0),
                    filter: filter.map(|_| false),
                    check: false,
                });
            };
            let confirmed = last.position;
            let selected =
                wallet_index_balance::db::select(connection, &network, &selector, confirmed)
                    .context("the selector did not read")?;
            let fees = Fees::new(sources.estimator, runtime);
            let mut evaluation =
                Evaluation::new(connection, sources.now, None, sources.decimals, fees);
            let filter = filter
                .map(|filter| evaluation.filter(&filter, &selected))
                .transpose()?;
            let check = evaluation.check(&check, &selected)?;
            Ok(Preview {
                confirmed: Some(confirmed),
                selected,
                filter,
                check,
            })
        })
        .await
    }
}
