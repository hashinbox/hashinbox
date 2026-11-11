//! Triggers (docs/design/065-pipes.md, eval.md): the [`Round`] job, one
//! round of a network's triggers, queued by the index job at the tip. Each
//! trigger decides in `usecase::trigger`; this worker runs its action.
//!
//! A round's topic is the whole network (`chain:network`), so it never runs
//! beside a broadcast of that network, and the jobs it starts run after it:
//! every round sees what earlier rounds sent as pending or mined, never in
//! flight. It is unique: at most one round of a network is pending or
//! running. It gives way (`OnLock::Skip`) to a job of its network queued
//! after it, such as a send from the API: it would decide without seeing it.

pub(crate) mod actions;

use std::{num::NonZeroUsize, sync::Arc};

use logging::{Level, Logger};
use payments::Payments;
use tasks::{Ack, OnLock, Policy, Topic, Worker};

pub(crate) use actions::{Actions, Invocation};

use super::{TIMEOUT, index::Target};
use crate::{
    db::Db,
    models::{
        Json, Task as Row, TriggerRun, task,
        trigger::{self, LastRound},
        trigger_run,
    },
    usecase::{
        tasks::Queue,
        trigger::{Context as Snapshot, Triggers},
    },
};

/// The job kind.
pub(crate) const KIND: &str = "triggers";

/// Runs a network's triggers: the triggers job.
pub(crate) struct Round {
    payments: Arc<Payments>,
    db: Db,
    queue: Queue,
    actions: Arc<Actions>,
    logger: Logger,
}

impl Round {
    pub(crate) fn new(payments: Arc<Payments>, db: Db, queue: Queue, logger: Logger) -> Self {
        Self {
            payments,
            db,
            queue,
            actions: Arc::new(Actions::new().add(actions::Transfer)),
            logger,
        }
    }
}

impl Worker for Round {
    const KIND: &'static str = KIND;
    /// One round per network unfinished, two networks' rounds at once; each
    /// gives way to a later job of its network.
    const POLICY: Policy = Policy {
        on_lock: OnLock::Skip,
        unique: true,
        concurrency: NonZeroUsize::new(2).unwrap(),
        timeout: TIMEOUT,
        ..Policy::DEFAULT
    };
    type Input = Target;
    type State = ();

    /// `chain:network`: the whole network.
    fn topic(target: &Target) -> Option<Topic> {
        Topic::new([target.segment()])
    }

    /// One round: every enabled trigger decides at the last kept block, on
    /// spendable balances (pending sends already taken off); each action's
    /// started jobs become one run with them, every run of the round kept
    /// in one write.
    async fn run(&self, cx: tasks::Context<'_, Target, ()>) -> Ack {
        let network = cx.input().network();
        let round = async {
            let now = i64::try_from(tasks::now())?;

            // Every enabled trigger decides on one snapshot.
            let service = self.payments.service(&network)?;
            let context = Snapshot {
                assets: service.assets.clone(),
                estimator: service.wallets.factory.fees(),
                runtime: tokio::runtime::Handle::current(),
                now,
            };
            let (deciding, actions) = (network.clone(), self.actions.clone());
            let (confirmed, verdicts) = self
                .db
                .read(move |connection| {
                    let triggers = Triggers::new(connection, &deciding)?;
                    let verdicts = triggers.decide(connection, &context, |decision| {
                        let mut invocation =
                            Invocation::new(decision.network, decision.this, decision.trigger);
                        let action = decision.action;
                        actions.run(&action.name, &action.input, &mut invocation)?;
                        Ok(invocation.started())
                    });
                    Ok((triggers.confirmed(), verdicts))
                })
                .await?;
            let Some(confirmed) = confirmed else {
                return anyhow::Ok(Ack::Idle);
            };

            // What each trigger saw; refusals are logged too, and the rest
            // that started jobs act.
            let mut rounds = Vec::new();
            let mut acting = Vec::new();
            for (id, mut stats, verdict) in verdicts {
                let mut refused = None;
                match verdict {
                    Ok(Some(jobs)) if !jobs.is_empty() => {
                        stats.jobs = jobs.len();
                        acting.push((id.clone(), stats, jobs));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let why = format!("{error:#}");
                        self.logger.event(
                            Level::Warn,
                            "trigger.refused",
                            &format!("{id}: {why}"),
                            &[
                                ("chain", network.chain.as_str()),
                                ("network", &network.name),
                            ],
                        );
                        refused = Some(why);
                    }
                }
                let round = LastRound {
                    at: now,
                    confirmed,
                    stats,
                    refused,
                };
                rounds.push((id, round));
            }
            if rounds.is_empty() {
                return anyhow::Ok(Ack::Idle);
            }

            // What every trigger saw, and every acting trigger's jobs and
            // run, in one write: all or none, so a rerun never decides again
            // what this one kept. The one write to the queue outside its
            // store, allowed because the jobs are not unique: each is
            // admitted.
            let acted = !acting.is_empty();
            self.db
                .write(move |connection| {
                    for (id, round) in &rounds {
                        trigger::db::set_last_round(connection, id, round)?;
                    }
                    for (id, stats, jobs) in &acting {
                        let mut ids = Vec::new();
                        for job in jobs {
                            let queued = task::db::insert(connection, &Row::try_from(job)?, now)?;
                            ids.push(queued.to_string());
                        }
                        let run = TriggerRun {
                            jobs: Json(ids),
                            stats: Json(*stats),
                            ..TriggerRun::new(id, now)
                        };
                        trigger_run::db::insert(connection, &run)?;
                    }
                    Ok(())
                })
                .await?;
            if !acted {
                return anyhow::Ok(Ack::Idle);
            }
            self.queue.notify();
            anyhow::Ok(Ack::Done)
        };
        match round.await {
            Ok(ack) => ack,
            Err(error) => Ack::Retry(format!("{error:#}")),
        }
    }
}
