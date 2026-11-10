//! The prune job: one, never finished. Every [`PERIOD`] it deletes the
//! recurring jobs ([`RECURRING`]) that succeeded, or were skipped, more than
//! [`KEEP`] ago. Such a triggers job holds nothing that is not kept elsewhere (its runs
//! in `trigger_run`), so its row is only noise in the queue. Other jobs stay
//! (a broadcast is the record of a send), as do failed ones: their reason is
//! what tells why something did not happen.
//!
//! Its `before` enqueues it when its queue starts; it is unique. Its topic (`prune`)
//! conflicts with nothing else.

use std::time::Duration;

use tasks::{Ack, Context, Policy, Store, StoreError, Topic, Worker, millis};

use super::{FOREVER, TIMEOUT, trigger};
use crate::{db::Db, models::task};

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
    db: Db,
}

impl Prune {
    pub(crate) const fn new(db: Db) -> Self {
        Self { db }
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

    /// Deletes what succeeded before [`KEEP`] ago; a failed write retries.
    async fn run(&self, _cx: Context<'_, (), ()>) -> Ack {
        let Ok(before) = i64::try_from(tasks::now().saturating_sub(millis(KEEP))) else {
            return Ack::Fail("now is above range".into());
        };
        let pruned = self
            .db
            .write(move |connection| {
                Ok(task::db::delete_done_before(
                    connection, &RECURRING, before,
                )?)
            })
            .await;
        match pruned {
            Ok(_) => Ack::Again(tasks::now().saturating_add(millis(PERIOD)), None),
            Err(error) => Ack::Retry(format!("{error:#}")),
        }
    }

    /// The one prune job.
    async fn before(&self, store: &dyn Store) -> Result<(), StoreError> {
        Self::enqueue(store, &()).await.map(|_| ())
    }
}
