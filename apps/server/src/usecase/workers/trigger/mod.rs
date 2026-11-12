//! Triggers (docs/design/065-pipes.md, 077): the [`Round`] job, one round
//! of a network's triggers, queued by the index job at the tip. The round
//! itself is the `trigger` package's; this worker reads it over SQLite,
//! writes what it decided, and its actions are [`actions`].
//!
//! A round's topic is the whole network (`chain:network`), so it never runs
//! beside a broadcast of that network, and the jobs it starts run after it:
//! every round sees what earlier rounds sent as pending or mined, never in
//! flight. It is unique: at most one round of a network is pending or
//! running. It gives way (`OnLock::Skip`) to a job of its network queued
//! after it, such as a send from the API: it would decide without seeing it.

pub(crate) mod actions;

use std::{num::NonZeroUsize, sync::Arc};

use ::trigger::{Decision, actions::Registry};
use diesel::SqliteConnection;
use logging::{Level, Logger};
use payments::Payments;
use tasks::{Ack, Job, OnLock, Policy, Topic, Worker};

use super::{TIMEOUT, index::Target};
use crate::{
    db::Db,
    models::{Json, Task as Row, TriggerRun, int, network_index, task, trigger, trigger_run, uint},
    usecase::{tasks::Queue, trigger::Sqlite},
};

/// The job kind.
pub(crate) const KIND: &str = "triggers";

/// Runs a network's triggers: the triggers job.
pub(crate) struct Round {
    payments: Arc<Payments>,
    db: Db,
    queue: Queue,
    actions: Arc<Registry<Job>>,
    logger: Logger,
}

impl Round {
    pub(crate) fn new(
        payments: Arc<Payments>,
        db: Db,
        queue: Queue,
        actions: Arc<Registry<Job>>,
        logger: Logger,
    ) -> Self {
        Self {
            payments,
            db,
            queue,
            actions,
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
    /// jobs become one run with them, every run of the round kept in one
    /// write.
    async fn run(&self, cx: tasks::Context<'_, Target, ()>) -> Ack {
        let network = cx.input().network();
        let round = async {
            let service = self.payments.service(&network)?;
            let (actions, fees, assets) = (
                self.actions.clone(),
                service.wallets.factory.fees(),
                service.assets.clone(),
            );
            let (now, runtime, at) = (
                tasks::now(),
                tokio::runtime::Handle::current(),
                network.clone(),
            );
            // One read: the confirmed position, the triggers and every
            // wallet read agree.
            let decided = self.db.read(move |connection| {
                let Some(confirmed) = network_index::db::find_confirmed(connection, &at)? else {
                    return Ok(None);
                };
                let triggers = trigger::db::find_enabled(connection, &at)?;
                let round = ::trigger::Round::new(
                    &actions,
                    fees,
                    &assets,
                    at.clone(),
                    uint(confirmed)?,
                    now,
                );
                let mut store = Sqlite::new(connection, at, confirmed);
                Ok(Some(runtime.block_on(round.run(&mut store, triggers))))
            });
            let Some(decided) = decided.await? else {
                return anyhow::Ok(Ack::Idle);
            };
            for one in &decided {
                let Some(why) = &one.last.refused else {
                    continue;
                };
                self.logger.event(
                    Level::Warn,
                    "trigger.refused",
                    &format!("{}: {why}", one.trigger),
                    &[
                        ("chain", network.chain.as_str()),
                        ("network", &network.name),
                    ],
                );
            }
            if decided.is_empty() {
                return anyhow::Ok(Ack::Idle);
            }
            // What every trigger saw, and every acting trigger's jobs and
            // run, in one write: all or none, so a rerun never decides again
            // what this one kept.
            let acted = decided.iter().any(|one| !one.effects.is_empty());
            self.db
                .write(move |connection| save(connection, now, &decided))
                .await?;
            if !acted {
                return anyhow::Ok(Ack::Idle);
            }
            self.queue.notify();
            anyhow::Ok(Ack::Done)
        };
        round
            .await
            .unwrap_or_else(|error| Ack::Retry(format!("{error:#}")))
    }
}

/// Every trigger's last round, and each acting one's jobs and run, at
/// `now`. The one write to the queue outside its store, allowed because
/// the jobs are not unique: each is admitted.
fn save(
    connection: &mut SqliteConnection,
    now: u64,
    decided: &[Decision<Job>],
) -> anyhow::Result<()> {
    let now = int(now)?;
    for one in decided {
        trigger::db::set_last_round(connection, &one.trigger, &one.last)?;
        if one.effects.is_empty() {
            continue;
        }
        let jobs = one
            .effects
            .iter()
            .map(|job| Ok(task::db::insert(connection, &Row::try_from(job)?, now)?.to_string()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let run = TriggerRun {
            jobs: Json(jobs),
            stats: Json(one.last.stats),
            ..TriggerRun::new(&one.trigger, now)
        };
        trigger_run::db::insert(connection, &run)?;
    }
    Ok(())
}
