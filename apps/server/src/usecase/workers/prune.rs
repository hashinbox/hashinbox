//! The prune job: one, never finished. Every [`PERIOD`] it deletes the
//! recurring jobs ([`RECURRING`]) that succeeded, or were skipped, more than
//! [`KEEP`] ago. Such a triggers job holds nothing that is not kept elsewhere (its runs
//! in `trigger_run`), so its row is only noise in the queue. Other jobs stay
//! (a broadcast is the record of a send), as do failed ones: their reason is
//! what tells why something did not happen. Then, per network, it deletes
//! the blocks below its line that nothing needs (`block::db::prune`), one
//! write per network: on a chain that reads balances, its indexer's last
//! final block; else its stored tip less its `max_depth`. With the jobs, it
//! deletes the kept `debug` and `trace` log entries older than
//! [`ROUTINE_KEPT`], and every one older than [`HISTORY_KEPT`]; then each
//! job's entries but its newest [`LOGS_PER_JOB`] (a long-lived index job is
//! capped, a payment keeps its whole story), and the entries of no job but
//! each network's (and no network's) newest [`LOGS_PER_NETWORK`].
//!
//! Its `before` enqueues it when its queue starts; it is unique. Its topic (`prune`)
//! conflicts with nothing else.

use std::{sync::Arc, time::Duration};

use tasks::{Ack, Context, Policy, Store, StoreError, Topic, Worker, millis};

use super::{FOREVER, TIMEOUT, trigger};
use crate::{
    db::Db,
    models::{Network, block, int, log, network, task},
    usecase::Transport,
};

/// The job kind.
pub(crate) const KIND: &str = "prune";

/// The kinds pruned: recurring jobs that finish, a new one each time. The
/// other recurring jobs (index, pending, prune) are one row each, never
/// finished.
const RECURRING: [&str; 1] = [trigger::KIND];

/// How often succeeded jobs are looked for.
const PERIOD: Duration = Duration::from_secs(60 * 60);

/// How long a succeeded job is kept after it last changed.
const KEEP: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a `debug` or `trace` log entry is kept.
const ROUTINE_KEPT: Duration = Duration::from_secs(24 * 60 * 60);

/// How long any other log entry is kept.
const HISTORY_KEPT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The most log entries one job keeps, newest first.
const LOGS_PER_JOB: i64 = 10_000;

/// The most log entries of no job one network keeps (and no network),
/// newest first.
const LOGS_PER_NETWORK: i64 = 10_000;

/// Deletes the jobs that succeeded long enough ago, and the blocks nothing
/// needs.
pub(crate) struct Prune {
    db: Db,
    container: Arc<base::Container<Transport>>,
}

impl Prune {
    pub(crate) const fn new(db: Db, container: Arc<base::Container<Transport>>) -> Self {
        Self { db, container }
    }

    /// Prunes the blocks of each network below its line ([`Self::line`]),
    /// one write per network.
    ///
    /// # Errors
    ///
    /// When a read or a write fails.
    pub(crate) async fn blocks(&self) -> anyhow::Result<()> {
        for network in self.db.read(network::db::find_all).await? {
            let Some(line) = self.line(&network) else {
                continue;
            };
            let (below, key) = (int(line)?, network.key());
            self.db
                .write(move |connection| Ok(block::db::prune(connection, &key, below)?))
                .await?;
        }
        Ok(())
    }

    /// Below what `network`'s blocks may go: on a chain that reads balances
    /// its indexer's last final block (nothing below it is read again);
    /// else its stored tip less its `max_depth`. `None`: not yet, or never.
    fn line(&self, network: &Network) -> Option<u64> {
        if let Ok(service) = self.container.service(&network.key())
            && service.balances.is_some()
        {
            return service.indexer.stats().confirmed;
        }
        let tip = network.index.as_ref()?.tip?;
        Some(tip.saturating_sub(network.max_depth?))
    }
}

impl Worker for Prune {
    const KIND: &'static str = KIND;
    /// One at all, retried for ever.
    const POLICY: Policy = Policy {
        unique: true,
        timeout: TIMEOUT,
        retry: FOREVER,
        ..Policy::DEFAULT
    };
    type Input = ();
    type State = ();

    /// `prune`: it conflicts with nothing else.
    fn topic((): &()) -> Option<Topic> {
        Topic::new([KIND])
    }

    /// Deletes what succeeded before [`KEEP`] ago, the log entries before
    /// [`ROUTINE_KEPT`] (`debug`, `trace`) or [`HISTORY_KEPT`] ago and those
    /// past [`LOGS_PER_JOB`] or [`LOGS_PER_NETWORK`], then prunes blocks ([`Self::blocks`]); a failed read or write
    /// retries.
    async fn run(&self, _cx: Context<'_, (), ()>) -> Ack {
        let now = tasks::now();
        let (Ok(before), Ok(routine_before), Ok(history_before)) = (
            i64::try_from(now.saturating_sub(millis(KEEP))),
            i64::try_from(now.saturating_sub(millis(ROUTINE_KEPT))),
            i64::try_from(now.saturating_sub(millis(HISTORY_KEPT))),
        ) else {
            return Ack::Fail("now is above range".into());
        };
        let pruned = async {
            self.db
                .write(move |connection| {
                    task::db::delete_done_before(connection, &RECURRING, before)?;
                    log::db::prune(connection, routine_before, history_before)?;
                    log::db::delete_beyond_per_job(connection, LOGS_PER_JOB)?;
                    log::db::delete_jobless_beyond_per_network(connection, LOGS_PER_NETWORK)?;
                    Ok(())
                })
                .await?;
            self.blocks().await
        }
        .await;
        match pruned {
            Ok(()) => Ack::Again(tasks::now().saturating_add(millis(PERIOD)), None),
            Err(error) => Ack::Retry(format!("{error:#}")),
        }
    }

    /// The one prune job.
    async fn before(&self, store: &dyn Store) -> Result<(), StoreError> {
        Self::enqueue(store, &()).await.map(|_| ())
    }
}
