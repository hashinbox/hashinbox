//! `q`: one row per queued job for its whole life (`init.sql`). The
//! statements behind `tasks::Store` (`crate::usecase::tasks`), the queue's
//! rules among them: line order, conflicts, skips, expiry and leases.

use anyhow::Context as _;
use diesel::{prelude::*, sqlite::Sqlite};
use tasks::{Id, Job, Status, Topic};

use super::{int, schema::q, uint};

/// One queued job as written; its id is the row's.
#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = q, check_for_backend(Sqlite), treat_none_as_null = true)]
pub(crate) struct Task {
    pub(crate) topic: String,
    pub(crate) kind: String,
    pub(crate) status: i32,
    pub(crate) run_at: i64,
    pub(crate) deadline: Option<i64>,
    pub(crate) attempts: i32,
    pub(crate) reason: Option<String>,
    pub(crate) input: String,
    pub(crate) state: Option<String>,
}

/// `status` as stored.
pub(crate) const fn status(status: Status) -> i32 {
    match status {
        Status::Pending => 0,
        Status::Running => 1,
        Status::Done => 2,
        Status::Failed => 3,
        Status::Skipped => 4,
        Status::Idle => 5,
    }
}

/// A stored `status`; an unknown one reads as failed.
pub(crate) const fn status_of(status: i32) -> Status {
    match status {
        0 => Status::Pending,
        1 => Status::Running,
        2 => Status::Done,
        4 => Status::Skipped,
        5 => Status::Idle,
        _ => Status::Failed,
    }
}

/// The row of a job. Fails when a time or the attempts are above SQLite's
/// integer range.
impl TryFrom<&Job> for Task {
    type Error = anyhow::Error;

    fn try_from(job: &Job) -> anyhow::Result<Self> {
        Ok(Self {
            topic: job.topic.to_string(),
            kind: job.kind.clone(),
            status: status(job.status),
            run_at: int(job.run_at)?,
            deadline: job.deadline.map(int).transpose()?,
            attempts: i32::try_from(job.attempts).context("attempts are above range")?,
            reason: job.reason.clone(),
            input: job.input.to_string(),
            state: job.state.as_ref().map(ToString::to_string),
        })
    }
}

impl Task {
    /// The job of row `id`. Fails when a stored value does not read back.
    pub(crate) fn into_job(self, id: i64) -> anyhow::Result<Job> {
        let row = self;
        Ok(Job {
            id: Id(uint(id)?),
            topic: Topic::parse(&row.topic).context("stored topic")?,
            kind: row.kind,
            status: status_of(row.status),
            run_at: uint(row.run_at)?,
            deadline: row.deadline.map(uint).transpose()?,
            attempts: u32::try_from(row.attempts).context("stored attempts are negative")?,
            reason: row.reason,
            input: serde_json::from_str(&row.input).context("stored input")?,
            state: row
                .state
                .map(|state| serde_json::from_str(&state))
                .transpose()
                .context("stored state")?,
        })
    }
}

/// The `q` table's statements.
pub(crate) mod db {
    use std::collections::BTreeMap;

    use diesel::{
        prelude::*,
        sql_types::{BigInt, Bool, Text},
    };

    use super::Task;
    use crate::{db::Page, models::schema::q};

    /// Inserts `task` stamped `now`; its id, the next place in line.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        task: &Task,
        now: i64,
    ) -> QueryResult<i64> {
        diesel::insert_into(q::table)
            .values((task, q::created_at.eq(now), q::updated_at.eq(now)))
            .returning(q::id)
            .get_result(connection)
    }

    /// The unfinished job of `kind` on `topic`, if any.
    pub(crate) fn find_unfinished(
        connection: &mut SqliteConnection,
        kind: &str,
        topic: &str,
    ) -> QueryResult<Option<i64>> {
        q::table
            .filter(q::kind.eq(kind))
            .filter(q::topic.eq(topic))
            .filter(q::status.eq_any([0, 1]))
            .select(q::id)
            .first(connection)
            .optional()
    }

    /// Writes `task` on job `id`, stamped `now`, only while it runs the
    /// claim of `attempt`; whether it did.
    pub(crate) fn update(
        connection: &mut SqliteConnection,
        id: i64,
        attempt: i32,
        task: &Task,
        now: i64,
    ) -> QueryResult<bool> {
        let running = q::table
            .find(id)
            .filter(q::status.eq(1))
            .filter(q::attempts.eq(attempt));
        diesel::update(running)
            .set((task, q::updated_at.eq(now)))
            .execute(connection)
            .map(|rows| rows > 0)
    }

    /// Whether `kind` has a job due at `now` (pending or running, its
    /// `run_at` reached): whether a claim has anything to do.
    pub(crate) fn any_due(
        connection: &mut SqliteConnection,
        kind: &str,
        now: i64,
    ) -> QueryResult<bool> {
        let due = q::table
            .filter(q::kind.eq(kind))
            .filter(q::status.eq_any([0, 1]))
            .filter(q::run_at.le(now));
        diesel::select(diesel::dsl::exists(due)).get_result(connection)
    }

    /// Marks `kind`'s jobs past their deadline at `now`, and not under a
    /// lease, failed `expired`; how many.
    pub(crate) fn expire(
        connection: &mut SqliteConnection,
        kind: &str,
        now: i64,
    ) -> QueryResult<usize> {
        let unleased = q::status.eq(0).or(q::status.eq(1).and(q::run_at.le(now)));
        let past = q::table
            .filter(q::kind.eq(kind))
            .filter(q::deadline.lt(now))
            .filter(unleased);
        diesel::update(past)
            .set((
                q::status.eq(3),
                q::reason.eq("expired"),
                q::updated_at.eq(now),
            ))
            .execute(connection)
    }

    /// `kind`'s jobs due at `now`, oldest first, each with the first
    /// conflicting job blocking it (`BLOCKS`), if any.
    pub(crate) fn due(
        connection: &mut SqliteConnection,
        kind: &str,
        now: i64,
    ) -> QueryResult<Vec<(i64, Option<i64>)>> {
        /// A due job and its blocker.
        #[derive(QueryableByName)]
        struct Due {
            #[diesel(sql_type = BigInt)]
            id: i64,
            #[diesel(sql_type = diesel::sql_types::Nullable<BigInt>)]
            blocker: Option<i64>,
        }

        let due: Vec<Due> = diesel::sql_query(
            "SELECT
                job.id,
                (SELECT MIN(other.id) FROM q other
                 WHERE other.status IN (0, 1)
                   AND CONFLICTS(other.topic, job.topic)
                   AND BLOCKS(other.id, other.status, other.run_at, other.attempts, job.id, ?2)
                ) AS blocker
             FROM q job
             WHERE job.kind = ?1 AND job.status IN (0, 1) AND job.run_at <= ?2
             ORDER BY job.id",
        )
        .bind::<Text, _>(kind)
        .bind::<BigInt, _>(now)
        .load(connection)?;
        Ok(due.into_iter().map(|job| (job.id, job.blocker)).collect())
    }

    /// Marks job `id` skipped at `now`, giving way to `holder`.
    pub(crate) fn skip(
        connection: &mut SqliteConnection,
        id: i64,
        holder: i64,
        now: i64,
    ) -> QueryResult<usize> {
        diesel::update(q::table.find(id))
            .set((
                q::status.eq(4),
                q::reason.eq(format!("skipped: {holder}")),
                q::updated_at.eq(now),
            ))
            .execute(connection)
    }

    /// Marks jobs `ids` running until `until`, their attempt counted; them,
    /// oldest first.
    pub(crate) fn lease(
        connection: &mut SqliteConnection,
        ids: &[i64],
        now: i64,
        until: i64,
    ) -> QueryResult<Vec<(i64, Task)>> {
        let mut leased: Vec<(i64, Task)> = diesel::update(q::table.filter(q::id.eq_any(ids)))
            .set((
                q::status.eq(1),
                q::attempts.eq(q::attempts + 1),
                q::run_at.eq(until),
                q::updated_at.eq(now),
            ))
            .returning((q::id, Task::as_returning()))
            .get_results(connection)?;
        leased.sort_by_key(|(id, _)| *id);
        Ok(leased)
    }

    /// The job `id`, if any, with its id, `created_at` and `updated_at`.
    pub(crate) fn find_by_id(
        connection: &mut SqliteConnection,
        id: i64,
    ) -> QueryResult<Option<(i64, Task, i64, i64)>> {
        q::table
            .find(id)
            .select((q::id, Task::as_select(), q::created_at, q::updated_at))
            .first(connection)
            .optional()
    }

    /// Why the last job of `kind` on each topic failed, by topic: only the
    /// topics whose last job has a `reason`.
    pub(crate) fn find_last_reasons(
        connection: &mut SqliteConnection,
        kind: &str,
    ) -> QueryResult<BTreeMap<String, String>> {
        use diesel::dsl::sql;
        let rows = q::table
            .filter(
                sql::<Bool>("id IN (SELECT MAX(id) FROM q WHERE kind = ")
                    .bind::<Text, _>(kind)
                    .sql(" GROUP BY topic)"),
            )
            .filter(q::reason.is_not_null())
            .select((q::topic, q::reason.assume_not_null()))
            .load::<(String, String)>(connection)?;
        Ok(rows.into_iter().collect())
    }

    /// Deletes every job of one of `kinds` that succeeded, was idle or was
    /// skipped and was last written before `before` (Unix ms); how many.
    pub(crate) fn delete_done_before(
        connection: &mut SqliteConnection,
        kinds: &[&str],
        before: i64,
    ) -> QueryResult<usize> {
        diesel::delete(
            q::table
                .filter(q::kind.eq_any(kinds))
                .filter(q::status.eq_any([2, 4, 5]))
                .filter(q::updated_at.lt(before)),
        )
        .execute(connection)
    }

    /// How many jobs of `kind` are failing: failed, or not succeeded with
    /// a `reason` (their last run asked to go again because of it).
    pub(crate) fn count_failing(connection: &mut SqliteConnection, kind: &str) -> QueryResult<i64> {
        q::table
            .filter(q::kind.eq(kind))
            .filter(q::status.ne(2))
            .filter(q::reason.is_not_null())
            .count()
            .get_result(connection)
    }

    /// How many jobs are in each status, as (status, count).
    pub(crate) fn count_by_status(
        connection: &mut SqliteConnection,
    ) -> QueryResult<Vec<(i32, i64)>> {
        q::table
            .group_by(q::status)
            .select((q::status, diesel::dsl::count_star()))
            .load(connection)
    }

    /// How many jobs finished (succeeded or failed) in each hour from
    /// `since` (Unix seconds) on, by their last write, as (hour, status,
    /// count); hour 0 starts at `since`.
    pub(crate) fn count_finished_by_hour(
        connection: &mut SqliteConnection,
        since: i64,
    ) -> QueryResult<Vec<(i64, i32, i64)>> {
        use diesel::{dsl::sql, sql_types::Integer};
        q::table
            .filter(q::status.eq_any([2, 3]))
            .filter(q::updated_at.ge(since * 1_000))
            .group_by(sql::<(BigInt, Integer)>("hour, status"))
            .select(
                sql::<(BigInt, Integer, BigInt)>("(updated_at / 1000 - ")
                    .bind::<BigInt, _>(since)
                    .sql(") / 3600 AS hour, status, COUNT(*)"),
            )
            .load(connection)
    }

    /// At most `page.limit` jobs last written, latest first, of `network`,
    /// `kind` and `status` when given (absent: every status but idle), each
    /// with its id, `created_at` and `updated_at`; by (`updated_at`, `id`),
    /// after `page.after` in that order when given.
    /// A job's network is its input's `chain` and `network`, which every
    /// job kind's input carries.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        network: Option<&base::Network>,
        kind: Option<&str>,
        status: Option<i32>,
        page: Page<(i64, i64)>,
    ) -> QueryResult<Vec<(i64, Task, i64, i64)>> {
        use diesel::dsl::sql;
        let mut query = q::table.into_boxed();
        if let Some(network) = network {
            query = query
                .filter(
                    sql::<Bool>("json_extract(input, '$.chain') = ")
                        .bind::<Text, _>(network.chain.to_string()),
                )
                .filter(
                    sql::<Bool>("json_extract(input, '$.network') = ")
                        .bind::<Text, _>(network.name.clone()),
                );
        }
        if let Some(kind) = kind {
            query = query.filter(q::kind.eq(kind));
        }
        query = match status {
            Some(status) => query.filter(q::status.eq(status)),
            None => query.filter(q::status.ne(super::status(tasks::Status::Idle))),
        };
        if let Some((updated_at, id)) = page.after {
            query = query.filter(
                sql::<Bool>("(updated_at, id) < (")
                    .bind::<BigInt, _>(updated_at)
                    .sql(", ")
                    .bind::<BigInt, _>(id)
                    .sql(")"),
            );
        }
        query
            .order((q::updated_at.desc(), q::id.desc()))
            .limit(page.limit)
            .select((q::id, Task::as_select(), q::created_at, q::updated_at))
            .load(connection)
    }
}
