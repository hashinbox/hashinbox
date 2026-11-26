//! What the store holds across every network (wallets, blocks,
//! transactions, triggers, jobs) and the last day hour by hour, whatever the
//! configuration names: counts and hourly series over ten models, in one
//! snapshot.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Error;
use crate::{
    db::Db,
    models::{block, leg, task, transaction, trigger, trigger_run, wallet, wallet_index},
};

/// How many hours a series covers, the current one last.
const HOURS: usize = 24;
const HOUR: i64 = 3_600;

/// Jobs by where they are.
#[derive(Default)]
pub(crate) struct Jobs {
    pub(crate) pending: u64,
    pub(crate) running: u64,
    pub(crate) succeeded: u64,
    pub(crate) failed: u64,
}

impl Jobs {
    /// The counts of (stored status, count) rows: skipped and idle jobs are
    /// in none of them; an unknown status counts as failed.
    fn of(counts: Vec<(i32, i64)>) -> Self {
        let mut jobs = Self::default();
        for (status, n) in counts {
            let n = u64::try_from(n).unwrap_or_default();
            let field = match task::status_of(status) {
                tasks::Status::Pending => &mut jobs.pending,
                tasks::Status::Running => &mut jobs.running,
                tasks::Status::Done => &mut jobs.succeeded,
                tasks::Status::Failed => &mut jobs.failed,
                tasks::Status::Skipped | tasks::Status::Idle => continue,
            };
            *field += n;
        }
        jobs
    }
}

/// One hour of chain time, across every network.
#[derive(Default)]
pub(crate) struct ChainHour {
    /// Its start, Unix seconds.
    pub(crate) start: i64,
    /// Live blocks stamped in it.
    pub(crate) blocks: u64,
    /// Kept transactions mined in those blocks.
    pub(crate) transactions: u64,
}

/// One hour of this server's time.
#[derive(Default)]
pub(crate) struct ServerHour {
    /// Its start, Unix seconds.
    pub(crate) start: i64,
    /// Jobs that succeeded in it.
    pub(crate) succeeded: u64,
    /// Jobs that failed for good in it.
    pub(crate) failed: u64,
    /// Trigger runs started in it.
    pub(crate) runs: u64,
}

/// The store's counts and its last day.
pub(crate) struct Stats {
    /// Wallets whose key is kept.
    pub(crate) wallets: u64,
    /// Distinct addresses indexed, keyed or watch-only.
    pub(crate) addresses: u64,
    /// Tags on addresses.
    pub(crate) tags: u64,
    /// Blocks kept.
    pub(crate) blocks: u64,
    /// Transactions kept mined.
    pub(crate) transactions: u64,
    /// Of those, how many reverted.
    pub(crate) reverted: u64,
    /// Transactions kept pending.
    pub(crate) pending: u64,
    /// Legs kept: movements and rewards.
    pub(crate) movements: u64,
    /// Triggers kept.
    pub(crate) triggers: u64,
    /// Of those, how many are enabled.
    pub(crate) enabled: u64,
    /// Jobs by where they are.
    pub(crate) jobs: Jobs,
    /// The last day of chain time (up to the newest kept block), hour by
    /// hour, oldest first.
    pub(crate) chain: Vec<ChainHour>,
    /// The last day of this server's time, hour by hour, oldest first.
    pub(crate) server: Vec<ServerHour>,
}

/// The start (Unix seconds) of the first of the `HOURS` hours ending with
/// the one `now` falls in.
fn first_hour(now: i64) -> i64 {
    (now / HOUR + 1 - HOURS as i64) * HOUR
}

/// The `HOURS` hours from `first` on, each `T` from its start.
fn hours<T>(first: i64, hour: impl Fn(i64) -> T) -> Vec<T> {
    (0..HOURS as i64).map(|i| hour(first + i * HOUR)).collect()
}

/// Hour `hour` (counted from the first) of `hours`, if among them.
fn at<T>(hours: &mut [T], hour: i64) -> Option<&mut T> {
    usize::try_from(hour)
        .ok()
        .and_then(|hour| hours.get_mut(hour))
}

/// The store's stats, by this server's clock.
///
/// # Errors
///
/// Internal when the clock is before 1970; the read.
pub(crate) async fn read(db: &Db) -> Result<Stats, Error> {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::internal(error.to_string()))?
        .as_secs();
    let clock = i64::try_from(clock).map_err(|error| Error::internal(error.to_string()))?;
    let stats = db.read(move |connection| {
        let count = |value: i64| u64::try_from(value).unwrap_or_default();
        let blocks = block::db::count(connection)?;
        let (transactions, reverted, pending) = transaction::db::count(connection)?;
        let (triggers, enabled) = trigger::db::count(connection)?;
        let jobs = Jobs::of(task::db::count_by_status(connection)?);

        // Chain time ends at the newest kept block: a skewed clock or a
        // lagging indexer leaves no empty hours.
        let newest = block::db::find_newest_stamp(connection)?.unwrap_or(clock);
        let first = first_hour(newest);
        let mut chain = hours(first, |start| ChainHour {
            start,
            ..ChainHour::default()
        });
        for (hour, n) in block::db::count_by_hour(connection, first)? {
            if let Some(one) = at(&mut chain, hour) {
                one.blocks = count(n);
            }
        }
        for (hour, n) in transaction::db::count_by_hour(connection, first)? {
            if let Some(one) = at(&mut chain, hour) {
                one.transactions = count(n);
            }
        }

        let first = first_hour(clock);
        let mut server = hours(first, |start| ServerHour {
            start,
            ..ServerHour::default()
        });
        for (hour, status, n) in task::db::count_finished_by_hour(connection, first)? {
            if let Some(one) = at(&mut server, hour) {
                match status {
                    2 => one.succeeded = count(n),
                    _ => one.failed = count(n),
                }
            }
        }
        for (hour, n) in trigger_run::db::count_by_hour(connection, first)? {
            if let Some(one) = at(&mut server, hour) {
                one.runs = count(n);
            }
        }

        Ok(Stats {
            wallets: count(wallet::db::count_keyed(connection)?),
            addresses: count(wallet_index::db::count_addresses(connection)?),
            tags: count(wallet::db::count_tags(connection)?),
            blocks: count(blocks),
            transactions: count(transactions),
            reverted: count(reverted),
            pending: count(pending),
            movements: count(leg::db::count(connection)?),
            triggers: count(triggers),
            enabled: count(enabled),
            jobs,
            chain,
            server,
        })
    });
    Ok(stats.await?)
}

#[cfg(test)]
mod tests {
    use super::Jobs;

    #[test]
    fn idle_and_skipped_jobs_are_not_failed() {
        let jobs = Jobs::of(vec![
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 25),
            (4, 7),
            (5, 691),
            (9, 1),
        ]);
        let counts = (jobs.pending, jobs.running, jobs.succeeded, jobs.failed);
        assert_eq!(counts, (1, 2, 3, 26), "an unknown status is failed, summed");
    }
}
