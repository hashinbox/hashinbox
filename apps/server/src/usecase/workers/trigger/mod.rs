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
//!
//! A round runs in its network's span, each trigger in a `trigger` one
//! (`trigger_id`), and tells what it did: per trigger `trigger.fired` (info,
//! one per payment queued, under that payment's `job_id`), `trigger.skipped`
//! (debug, why) or `trigger.refused` (warn); then `trigger.round.acted` (info,
//! when one fired or was refused) or `trigger.round.idle` (debug) with its
//! counts.

pub(crate) mod actions;

use std::{num::NonZeroUsize, sync::Arc, time::Instant};

use ::trigger::{Decision, actions::Registry};
use diesel::SqliteConnection;
use tasks::{Ack, Job, OnLock, Policy, Topic, Worker};
use tracing::Instrument as _;

use super::{TIMEOUT, broadcast::SendIntent, index::Target};
use crate::{
    db::Db,
    models::{Json, Task as Row, TriggerRun, int, network_index, task, trigger, trigger_run, uint},
    usecase::{tasks::Queue, trigger::Sqlite},
};

/// The job kind.
pub(crate) const KIND: &str = "triggers";

/// Runs a network's triggers: the triggers job.
pub(crate) struct Round {
    container: Arc<base::Container<crate::usecase::Transport>>,
    db: Db,
    queue: Queue,
    actions: Arc<Registry<Job>>,
}

impl Round {
    pub(crate) fn new(
        container: Arc<base::Container<crate::usecase::Transport>>,
        db: Db,
        queue: Queue,
        actions: Arc<Registry<Job>>,
    ) -> Self {
        Self {
            container,
            db,
            queue,
            actions,
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
        let span = crate::usecase::network_span(&network);
        let round = async {
            let started = Instant::now();
            let service = self.container.service(&network)?;
            let (actions, fees, assets) = (
                self.actions.clone(),
                service.wallets.factory.fees(),
                service.assets.clone(),
            );
            let (now, runtime, at, span) = (
                tasks::now(),
                tokio::runtime::Handle::current(),
                network.clone(),
                tracing::Span::current(),
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
                let decided = round.run(&mut store, triggers).instrument(span);
                Ok(Some(runtime.block_on(decided)))
            });
            let Some(decided) = decided.await? else {
                return anyhow::Ok(Ack::Idle);
            };
            let (evaluated, mut fired, mut refused, mut wallets) = (decided.len(), 0, 0, 0);
            let mut payments = Vec::new();
            for one in &decided {
                let (trigger_id, stats) = (one.trigger.as_str(), one.last.stats);
                wallets += stats.selected;
                if let Some(reason) = &one.last.refused {
                    refused += 1;
                    tracing::warn!(target: crate::events::TRIGGER_REFUSED, trigger_id, reason);
                    continue;
                }
                if one.effects.is_empty() {
                    let reason = if stats.selected == 0 {
                        "nothing selected"
                    } else if stats.kept == 0 {
                        "filter kept none"
                    } else if !stats.checked {
                        "check did not hold"
                    } else {
                        "action queued nothing"
                    };
                    tracing::debug!(target: crate::events::TRIGGER_SKIPPED, trigger_id, reason);
                    continue;
                }
                fired += 1;
                let sends = one
                    .effects
                    .iter()
                    .map(|job| (one.trigger.clone(), job.input.clone()));
                payments.extend(sends);
            }
            // What every trigger saw, and every acting trigger's jobs and
            // run, in one write: all or none, so a rerun never decides again
            // what this one kept.
            let jobs = if decided.is_empty() {
                Vec::new()
            } else {
                let saved = self
                    .db
                    .write(move |connection| save(connection, now, &decided));
                saved.await?
            };
            for ((trigger_id, input), job_id) in payments.into_iter().zip(&jobs) {
                let send = serde_json::from_value::<SendIntent>(input).ok();
                let (wallet, asset, amount) = send
                    .map(|send| (send.address, send.asset, send.amount))
                    .unwrap_or_default();
                tracing::info!(
                    target: crate::events::TRIGGER_FIRED,
                    trigger_id,
                    wallet,
                    asset,
                    amount,
                    job_id
                );
            }
            let skipped = evaluated - fired - refused;
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            if fired + refused > 0 {
                tracing::info!(
                    target: crate::events::TRIGGER_ROUND_ACTED,
                    evaluated, fired, skipped, refused, wallets, ms
                );
            } else {
                tracing::debug!(
                    target: crate::events::TRIGGER_ROUND_IDLE,
                    evaluated, fired, skipped, refused, wallets, ms
                );
            }
            if jobs.is_empty() {
                return anyhow::Ok(Ack::Idle);
            }
            self.queue.notify();
            anyhow::Ok(Ack::Done)
        };
        round
            .instrument(span)
            .await
            .unwrap_or_else(|error| Ack::Retry(format!("{error:#}")))
    }
}

/// Every trigger's last round, and each acting one's jobs and run, at
/// `now`; the jobs' ids, in decision then effect order. The one write to
/// the queue outside its store, allowed because the jobs are not unique:
/// each is admitted.
fn save(
    connection: &mut SqliteConnection,
    now: u64,
    decided: &[Decision<Job>],
) -> anyhow::Result<Vec<i64>> {
    let now = int(now)?;
    let mut queued = Vec::new();
    for one in decided {
        trigger::db::set_last_round(connection, &one.trigger, &one.last)?;
        if one.effects.is_empty() {
            continue;
        }
        let jobs = one
            .effects
            .iter()
            .map(|job| Ok(task::db::insert(connection, &Row::try_from(job)?, now)?))
            .collect::<anyhow::Result<Vec<_>>>()?;
        queued.extend(&jobs);
        let run = TriggerRun {
            jobs: Json(jobs.iter().map(ToString::to_string).collect()),
            stats: Json(one.last.stats),
            ..TriggerRun::new(&one.trigger, now)
        };
        trigger_run::db::insert(connection, &run)?;
    }
    Ok(queued)
}
