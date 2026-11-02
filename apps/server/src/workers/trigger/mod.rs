//! Triggers (docs/design/065-pipes.md, eval.md): the [`Triggers`] job, one
//! round of a network's triggers, queued by [`Listen`] as blocks are kept.
//!
//! A round's topic is the whole network (`chain:network`), so it never runs
//! beside a broadcast of that network, and the jobs it starts run after it:
//! every round sees what earlier rounds sent as pending or mined, never in
//! flight.

pub(crate) mod actions;
mod evaluation;
mod listen;
mod run;
mod touched;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

use base::{Address, Chain, Network};
use logging::{Level, Logger};
use payments::Payments;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tasks::{Ack, Job, Task, Topic, Worker};

pub(crate) use actions::{Actions, Context};
pub(crate) use evaluation::Evaluation;
pub(crate) use listen::Listen;
#[cfg(test)]
pub(crate) use {run::settle, touched::touched};

use crate::{
    models::{
        Json, Task as Row, TriggerRun, block, task, transaction, trigger, trigger_run, wallet,
    },
    models::{wallet_index, wallet_index::Selector, wallet_index_balance},
    storage::Storage,
};

/// The job kind.
pub(crate) const KIND: &str = "triggers";

/// What a round looks at.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Scope {
    /// The network's chain.
    pub(crate) chain: String,
    /// The network.
    pub(crate) network: String,
    /// Ids of the kept blocks to look at; `None`: every trigger decides.
    pub(crate) blocks: Option<Vec<String>>,
}

impl Scope {
    /// The round job of this scope, locking its whole network.
    pub(crate) fn job(&self) -> anyhow::Result<Job> {
        let topic = Topic::new([format!("{}:{}", self.chain, self.network)])
            .ok_or_else(|| anyhow::anyhow!("the network does not make a topic"))?;
        Ok(Job::new(topic, KIND, json!(self)))
    }
}

/// What one trigger decided.
enum Verdict {
    /// Its check passed and its action started jobs: start a run with them.
    Act(Vec<Job>),
    /// Its filter kept something but its check failed (busy, cooling down,
    /// below a threshold): ask again on the network's next block.
    Held,
    /// Nothing to do until something it selects changes.
    Rest,
}

/// Runs a network's triggers.
pub(crate) struct Triggers {
    payments: Arc<Payments>,
    storage: Storage,
    logger: Logger,
    actions: Arc<Actions>,
    /// Per network, the triggers held back at their last decision.
    held: Mutex<BTreeMap<Network, BTreeSet<String>>>,
}

impl Triggers {
    pub(crate) fn new(payments: Arc<Payments>, storage: Storage, logger: Logger) -> Self {
        Self {
            payments,
            storage,
            logger,
            actions: Arc::new(Actions::new().add(actions::Transfer)),
            held: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Worker for Triggers {
    const KIND: &'static str = KIND;
    type Input = Scope;
    type State = ();

    /// One round: forget expired pending transactions (their funds are free
    /// again) and settle done runs; then the triggers the blocks touched,
    /// the ones just settled and the ones held back (every one without
    /// blocks) decide at the last kept block; each action's started jobs
    /// become one run with them, kept together.
    async fn run(&self, task: Task<Scope, ()>) -> Ack {
        let scope = task.input();
        let network = Network::new(Chain::new(&scope.chain), &scope.network);
        let round = async {
            let now = i64::try_from(tasks::now())?;

            // Expire and settle, then which triggers decide.
            let (settling, blocks) = (network.clone(), scope.blocks.clone());
            let only = self
                .storage
                .write(move |connection| {
                    transaction::db::expire(connection, &settling, now)?;
                    let settled = run::settle(connection, &settling, now)?;
                    let Some(blocks) = blocks else {
                        return Ok(None);
                    };
                    let mut only = touched::touched(connection, &settling, &blocks)?;
                    only.extend(settled);
                    Ok(Some(only))
                })
                .await?;
            let held = || self.held.lock().unwrap_or_else(PoisonError::into_inner);
            let only = only.map(|mut only| {
                only.extend(held().get(&network).into_iter().flatten().cloned());
                only
            });
            if only.as_ref().is_some_and(BTreeSet::is_empty) {
                return Ok(());
            }

            // Each trigger's verdict at the last kept block. A wallet index
            // another enabled trigger also picks is left out (contested).
            let service = self.payments.service(&network)?;
            let (assets, estimator) = (service.assets.clone(), service.wallets.factory.fees());
            let (actions, runtime) = (self.actions.clone(), tokio::runtime::Handle::current());
            let (deciding, scoped) = (network.clone(), only.clone());
            let verdicts = self
                .storage
                .read(move |connection| {
                    let mut verdicts = BTreeMap::new();
                    let Some(last) = block::db::find_last(connection, &deciding)? else {
                        return Ok(verdicts);
                    };
                    let triggers = trigger::db::find_enabled(connection, &deciding)?;
                    let keyed: BTreeSet<Address> = wallet::db::find_all(connection)?
                        .addresses()
                        .into_iter()
                        .collect();
                    for one in triggers.iter() {
                        if scoped.as_ref().is_some_and(|only| !only.contains(&one.id)) {
                            continue;
                        }
                        let others: Vec<Selector> = triggers
                            .iter()
                            .filter(|other| other.id != one.id)
                            .map(|other| other.selector.0.clone())
                            .collect();
                        let mut verdict = || -> anyhow::Result<Verdict> {
                            let owned = wallet_index::db::select_owned(
                                connection,
                                &deciding,
                                &one.selector.0,
                                &others,
                                last.position,
                            )?
                            .owned;
                            let selected = wallet_index_balance::db::with_balances(
                                connection,
                                &deciding,
                                owned,
                                last.position,
                            )?;
                            let run = trigger_run::db::find_last(connection, &one.id)?;
                            let mut evaluation = Evaluation::new(
                                connection,
                                now,
                                run,
                                assets.clone(),
                                estimator.clone(),
                                runtime.clone(),
                            );
                            let kept = match &one.filter {
                                Some(filter) => evaluation.filter(&filter.0, &selected)?,
                                None => selected,
                            };
                            if !evaluation.check(&one.check.0, &kept)? {
                                return Ok(if kept.is_empty() {
                                    Verdict::Rest
                                } else {
                                    Verdict::Held
                                });
                            }
                            let mut context =
                                Context::new(&deciding, &kept, &mut evaluation, &keyed);
                            let action = &one.action.0;
                            actions.run(&action.name, &action.input, &mut context)?;
                            let jobs = context.started();
                            Ok(if jobs.is_empty() {
                                Verdict::Rest
                            } else {
                                Verdict::Act(jobs)
                            })
                        };
                        verdicts.insert(one.id.clone(), verdict());
                    }
                    Ok(verdicts)
                })
                .await?;

            // Remember who is held; collect who acts.
            let (mut acting, mut refused) = (Vec::new(), Vec::new());
            {
                let mut all = held();
                let held = all.entry(network.clone()).or_default();
                if only.is_none() {
                    held.clear();
                }
                for (id, verdict) in verdicts {
                    held.remove(&id);
                    match verdict {
                        Ok(Verdict::Act(jobs)) => acting.push((id, jobs)),
                        Ok(Verdict::Held) => {
                            held.insert(id);
                        }
                        Ok(Verdict::Rest) => {}
                        Err(error) => refused.push(format!("{id}: {error:#}")),
                    }
                }
            }
            for message in refused {
                self.logger.event(
                    Level::Warn,
                    "trigger.refused",
                    &message,
                    &[
                        ("chain", network.chain.as_str()),
                        ("network", &network.name),
                    ],
                );
            }

            // Each acting trigger's run and its jobs: both or neither.
            for (id, jobs) in acting {
                let run = TriggerRun {
                    jobs: Json(jobs.iter().map(|job| job.id.to_string()).collect()),
                    ..TriggerRun::new(&id, now)
                };
                self.storage
                    .write(move |connection| {
                        trigger_run::db::insert(connection, &run)?;
                        for job in &jobs {
                            task::db::insert(connection, &Row::try_from(job)?, now)?;
                        }
                        Ok(())
                    })
                    .await?;
                self.storage.announce();
            }
            anyhow::Ok(())
        };
        match round.await {
            Ok(()) => task.ack(),
            Err(error) => task.nack(format!("{error:#}")),
        }
    }
}
