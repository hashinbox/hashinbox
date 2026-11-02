//! The prune job: one, never finished. Every [`PERIOD`] it deletes the
//! recurring jobs ([`RECURRING`]) that succeeded more than [`KEEP`] ago. A
//! succeeded triggers job holds nothing that is not kept elsewhere (its runs
//! in `trigger_run`), so its row is only noise in the queue. Other jobs stay
//! (a broadcast is the record of a send), as do failed ones: their reason is
//! what tells why something did not happen.
//!
//! Its topic (`prune`) conflicts with nothing else.

use std::time::Duration;

use tasks::{Ack, Id, Job, Status, Store as _, Task, Topic, Worker};

use crate::{models::task, storage::Storage, workers::trigger};

/// The job kind.
pub(crate) const KIND: &str = "prune";

/// The kinds pruned: recurring jobs that finish, a new one each time. The
/// other recurring jobs (index, expire, prune) are one row each, never
/// finished.
const RECURRING: [&str; 1] = [trigger::KIND];

/// How often succeeded jobs are looked for.
const PERIOD: Duration = Duration::from_secs(60 * 60);

/// How long a succeeded job is kept after it last changed.
const KEEP: Duration = Duration::from_secs(24 * 60 * 60);

/// Deletes the jobs that succeeded long enough ago.
pub(crate) struct Prune {
    storage: Storage,
}

impl Prune {
    pub(crate) const fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// Makes sure the prune job exists; a finished one starts over.
    pub(crate) async fn start(&self) -> anyhow::Result<()> {
        let topic = Topic::new([KIND]).ok_or_else(|| anyhow::anyhow!("prune makes no topic"))?;
        let job = Job {
            id: Id::from(KIND.to_owned()),
            ..Job::new(topic, KIND, serde_json::Value::Null)
        };
        match self.storage.get(&job.id).await? {
            None => self.storage.enqueue(job).await?,
            Some(kept) if matches!(kept.status, Status::Done | Status::Failed) => {
                self.storage.update(job).await?;
            }
            Some(_) => {}
        }
        Ok(())
    }
}

impl Worker for Prune {
    const KIND: &'static str = KIND;
    type Input = ();
    type State = ();

    /// Deletes what succeeded before [`KEEP`] ago, then runs again a
    /// [`PERIOD`] later; a failed write retries.
    async fn run(&self, task: Task<(), ()>) -> Ack {
        let now = tasks::now();
        let keep = u64::try_from(KEEP.as_millis()).unwrap_or(u64::MAX);
        let Ok(before) = i64::try_from(now.saturating_sub(keep)) else {
            return task.fail("now is above range");
        };
        let pruned = self
            .storage
            .write(move |connection| {
                Ok(task::db::delete_succeeded_before(
                    connection, &RECURRING, before,
                )?)
            })
            .await;
        match pruned {
            Ok(_) => {
                let period = u64::try_from(PERIOD.as_millis()).unwrap_or(u64::MAX);
                task.again(now.saturating_add(period))
            }
            Err(error) => task.nack(format!("{error:#}")),
        }
    }
}
