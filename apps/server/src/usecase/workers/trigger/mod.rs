//! Triggers (docs/design/065-pipes.md, eval.md): the [`Round`] job, one
//! round of a network's triggers, queued by the index job at the tip. Each
//! trigger decides in `usecase::trigger`; this worker runs its action.
//!
//! A round's topic is the whole network (`chain:network`), so it never runs
//! beside a broadcast of that network, and the jobs it starts run after it:
//! every round sees what earlier rounds sent as pending or mined, never in
//! flight.

pub(crate) mod actions;

use std::sync::Arc;

use base::{Chain, Network};
use logging::{Level, Logger};
use payments::Payments;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tasks::{Ack, Job, Task, Topic, Worker};

pub(crate) use actions::{Actions, Invocation};

use crate::{
    db::Db,
    models::{Json, Task as Row, TriggerRun, task, trigger_run},
    usecase::{
        tasks::Queue,
        trigger::{Context, Triggers},
    },
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
}

impl Scope {
    /// The round job of this scope, locking its whole network.
    pub(crate) fn job(&self) -> anyhow::Result<Job> {
        let topic = Topic::new([format!("{}:{}", self.chain, self.network)])
            .ok_or_else(|| anyhow::anyhow!("the network does not make a topic"))?;
        Ok(Job::new(topic, KIND, json!(self)))
    }
}

/// Runs a network's triggers: the triggers job.
pub(crate) struct Round {
    payments: Arc<Payments>,
    db: Db,
    queue: Queue,
    logger: Logger,
    actions: Arc<Actions>,
}

impl Round {
    pub(crate) fn new(payments: Arc<Payments>, db: Db, queue: Queue, logger: Logger) -> Self {
        Self {
            payments,
            db,
            queue,
            logger,
            actions: Arc::new(Actions::new().add(actions::Transfer)),
        }
    }
}

impl Worker for Round {
    const KIND: &'static str = KIND;
    type Input = Scope;
    type State = ();

    /// One round: every enabled trigger decides at the last kept block, on
    /// spendable balances (pending sends already taken off); each action's
    /// started jobs become one run with them, kept together.
    async fn run(&self, task: Task<Scope, ()>) -> Ack {
        let scope = task.input();
        let network = Network::new(Chain::new(&scope.chain), &scope.network);
        let round = async {
            let now = i64::try_from(tasks::now())?;

            // Every enabled trigger decides on one snapshot.
            let service = self.payments.service(&network)?;
            let context = Context {
                assets: service.assets.clone(),
                estimator: service.wallets.factory.fees(),
                runtime: tokio::runtime::Handle::current(),
                now,
            };
            let (deciding, actions) = (network.clone(), self.actions.clone());
            let verdicts = self
                .db
                .read(move |connection| {
                    let triggers = Triggers::new(connection, &deciding)?;
                    Ok(triggers.decide(connection, &context, |decision| {
                        let mut invocation =
                            Invocation::new(decision.network, decision.this, decision.trigger);
                        let action = decision.action;
                        actions.run(&action.name, &action.input, &mut invocation)?;
                        Ok(invocation.started())
                    }))
                })
                .await?;

            // Refusals are logged; the rest that started jobs act.
            let mut acting = Vec::new();
            for (id, verdict) in verdicts {
                match verdict {
                    Ok(Some(jobs)) if !jobs.is_empty() => acting.push((id, jobs)),
                    Ok(_) => {}
                    Err(error) => self.logger.event(
                        Level::Warn,
                        "trigger.refused",
                        &format!("{id}: {error:#}"),
                        &[
                            ("chain", network.chain.as_str()),
                            ("network", &network.name),
                        ],
                    ),
                }
            }

            // Each acting trigger's run and its jobs: both or neither.
            for (id, jobs) in acting {
                let run = TriggerRun {
                    jobs: Json(jobs.iter().map(|job| job.id.to_string()).collect()),
                    ..TriggerRun::new(&id, now)
                };
                self.db
                    .write(move |connection| {
                        trigger_run::db::insert(connection, &run)?;
                        for job in &jobs {
                            task::db::insert(connection, &Row::try_from(job)?, now)?;
                        }
                        Ok(())
                    })
                    .await?;
                self.queue.announce();
            }
            anyhow::Ok(())
        };
        match round.await {
            Ok(()) => task.ack(),
            Err(error) => task.nack(format!("{error:#}")),
        }
    }
}
