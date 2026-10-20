//! One evaluation of a trigger at a network's last kept block: select what
//! it owns, filter, check, plan. [`decide`] does it for the triggers to run;
//! [`preview`] does it for an unsaved trigger, keeping every figure.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use anyhow::bail;
use base::{Address, Assets, FeeEstimator, Network};
use diesel::SqliteConnection;

use super::{Evaluation, Plan};
use crate::models::{
    Trigger, block, trigger,
    trigger::{Action, Filter, Gate, Logic},
    trigger_run, wallet,
    wallet_index::{self, Selector},
    wallet_index_balance::{self, WalletIndexBalances},
};

/// What a network's triggers are evaluated with, besides the connection.
#[derive(Clone)]
pub(crate) struct Sources {
    /// The network's assets.
    pub(crate) assets: Assets,
    pub(crate) estimator: Arc<dyn FeeEstimator>,
    /// Where a fee estimate is awaited (evaluations run on blocking threads).
    pub(crate) runtime: tokio::runtime::Handle,
    /// Unix ms.
    pub(crate) now: i64,
}

impl Sources {
    /// The sources of a network with assets `assets` and fee `estimator`, now.
    pub(crate) fn new(assets: Assets, estimator: Arc<dyn FeeEstimator>, now: i64) -> Self {
        Self {
            assets,
            estimator,
            runtime: tokio::runtime::Handle::current(),
            now,
        }
    }
}

/// What one trigger decided.
pub(crate) enum Verdict {
    /// Its check passed and it moves something: start a run.
    Act(Plan),
    /// Its filter kept something but its check failed (busy, cooling down,
    /// below a threshold): ask again on the network's next block.
    Held,
    /// Nothing to do until something it selects changes.
    Rest,
}

/// The verdict of each enabled trigger of `network` whose id is in `only`
/// (every one when `None`), at the last kept block. A wallet index another
/// enabled trigger also picks is left out (contested).
///
/// # Errors
///
/// When the triggers or the tip do not read; one trigger's failure is its
/// own entry.
pub(crate) fn decide(
    connection: &mut SqliteConnection,
    network: &Network,
    only: Option<&BTreeSet<String>>,
    sources: &Sources,
) -> anyhow::Result<BTreeMap<String, anyhow::Result<Verdict>>> {
    let Some(last) = block::db::find_last(connection, network)? else {
        return Ok(BTreeMap::new());
    };
    let triggers = trigger::db::find_enabled(connection, network)?;
    let keyed: BTreeSet<Address> = wallet::db::find_all(connection)?
        .addresses()
        .into_iter()
        .collect();
    let mut verdicts = BTreeMap::new();
    for one in triggers.iter() {
        if only.is_some_and(|only| !only.contains(&one.id)) {
            continue;
        }
        let others: Vec<Selector> = triggers
            .iter()
            .filter(|other| other.id != one.id)
            .map(|other| other.selector.0.clone())
            .collect();
        let verdict = verdict(
            connection,
            network,
            one,
            &others,
            last.position,
            &keyed,
            sources,
        );
        verdicts.insert(one.id.clone(), verdict);
    }
    Ok(verdicts)
}

/// `trigger`'s verdict at `confirmed`.
fn verdict(
    connection: &mut SqliteConnection,
    network: &Network,
    trigger: &Trigger,
    others: &[Selector],
    confirmed: i64,
    keyed: &BTreeSet<Address>,
    sources: &Sources,
) -> anyhow::Result<Verdict> {
    let owned = wallet_index::db::select_owned(
        connection,
        network,
        &trigger.selector.0,
        others,
        confirmed,
    )?
    .owned;
    let selected = wallet_index_balance::db::with_balances(connection, network, owned, confirmed)?;
    let run = trigger_run::db::find_last(connection, &trigger.id)?;
    let mut evaluation = Evaluation::new(
        connection,
        sources.now,
        run,
        sources.assets.clone(),
        sources.estimator.clone(),
        sources.runtime.clone(),
    );
    let kept = match &trigger.filter {
        Some(filter) => evaluation.filter(&filter.0, &selected)?,
        None => selected,
    };
    if !evaluation.check(&trigger.check.0, &kept)? {
        return Ok(if kept.is_empty() {
            Verdict::Rest
        } else {
            Verdict::Held
        });
    }
    let Action::Send(send) = &trigger.action.0;
    let plan = evaluation.plan(send, &kept)?;
    if plan.transfers.is_empty() {
        return Ok(Verdict::Rest);
    }
    if let Some(transfer) = plan.transfers.iter().find(|t| !keyed.contains(&t.from)) {
        bail!("source {} is not keyed here", transfer.from.value);
    }
    Ok(Verdict::Act(plan))
}

/// A trigger's targeting and action, unsaved.
pub(crate) struct Definition {
    pub(crate) selector: Selector,
    pub(crate) filter: Option<Logic<Filter>>,
    pub(crate) check: Logic<Gate>,
    pub(crate) action: Action,
}

/// What a trigger would see and do now.
pub(crate) struct Preview {
    /// The confirmed tip position read; `None` before the first block.
    pub(crate) confirmed: Option<i64>,
    /// What its selector picks, with balances.
    pub(crate) selected: WalletIndexBalances,
    /// What its filter keeps of them: `This` for the check and the action.
    pub(crate) kept: WalletIndexBalances,
    /// Whether its check holds over `kept`.
    pub(crate) check: bool,
    /// The transfers its action would make, or why it cannot.
    pub(crate) plan: Result<Plan, String>,
    /// Sources of the plan this server holds no key for; a run refuses them.
    pub(crate) unkeyed: Vec<Address>,
}

/// What a trigger on `network` defined by `definition` sees and would do at
/// the last kept block, as one that never ran; nothing is kept.
///
/// # Errors
///
/// When a read or the fee estimate fails.
pub(crate) fn preview(
    connection: &mut SqliteConnection,
    network: &Network,
    definition: &Definition,
    sources: &Sources,
) -> anyhow::Result<Preview> {
    let Some(last) = block::db::find_last(connection, network)? else {
        let none = WalletIndexBalances::new(network.clone(), 0);
        return Ok(Preview {
            confirmed: None,
            selected: none.clone(),
            kept: none,
            check: false,
            plan: Err("no block is kept yet".to_owned()),
            unkeyed: Vec::new(),
        });
    };
    let selected =
        wallet_index_balance::db::select(connection, network, &definition.selector, last.position)?;
    let keyed: BTreeSet<Address> = wallet::db::find_all(connection)?
        .addresses()
        .into_iter()
        .collect();
    let mut evaluation = Evaluation::new(
        connection,
        sources.now,
        None,
        sources.assets.clone(),
        sources.estimator.clone(),
        sources.runtime.clone(),
    );
    let kept = match &definition.filter {
        Some(filter) => evaluation.filter(filter, &selected)?,
        None => selected.clone(),
    };
    let check = evaluation.check(&definition.check, &kept)?;
    let Action::Send(send) = &definition.action;
    let plan = evaluation
        .plan(send, &kept)
        .map_err(|error| format!("{error:#}"));
    let unkeyed = plan
        .iter()
        .flat_map(|plan| &plan.transfers)
        .map(|transfer| transfer.from.clone())
        .filter(|from| !keyed.contains(from))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Preview {
        confirmed: Some(last.position),
        selected,
        kept,
        check,
        plan,
        unkeyed,
    })
}
