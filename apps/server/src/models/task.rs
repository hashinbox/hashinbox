//! `q`: one row per queued job for its whole life (`init.sql`). The
//! statements behind `tasks::Store` (`crate::usecase::tasks`).

use anyhow::Context as _;
use diesel::{prelude::*, sqlite::Sqlite};
use tasks::{Id, Job, Status, Topic};

use super::{int, schema::q, uint};

/// One queued job, as read and written (the stamps are the statements').
#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = q, check_for_backend(Sqlite), treat_none_as_null = true)]
pub(crate) struct Task {
    pub(crate) id: String,
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

/// The row of a job. Fails when a time or the attempts are above SQLite's
/// integer range.
impl TryFrom<&Job> for Task {
    type Error = anyhow::Error;

    fn try_from(job: &Job) -> anyhow::Result<Self> {
        Ok(Self {
            id: job.id.to_string(),
            topic: job.topic.to_string(),
            kind: job.kind.clone(),
            status: match job.status {
                Status::Pending => 0,
                Status::Running => 1,
                Status::Done => 2,
                Status::Failed => 3,
            },
            run_at: int(job.run_at)?,
            deadline: job.deadline.map(int).transpose()?,
            attempts: i32::try_from(job.attempts).context("attempts are above range")?,
            reason: job.reason.clone(),
            input: job.input.to_string(),
            state: job.state.as_ref().map(ToString::to_string),
        })
    }
}

/// A row as its job. Fails when a stored value does not read back.
impl TryFrom<Task> for Job {
    type Error = anyhow::Error;

    fn try_from(row: Task) -> anyhow::Result<Self> {
        Ok(Self {
            id: Id::from(row.id),
            topic: Topic::parse(&row.topic).context("stored topic")?,
            kind: row.kind,
            status: match row.status {
                0 => Status::Pending,
                1 => Status::Running,
                2 => Status::Done,
                _ => Status::Failed,
            },
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

collection! {
    /// Tasks, ordered by id.
    Tasks(Task)
}

/// A job with its `updated_at`: `"{updated_at}:{id}"`.
impl crate::db::Keyed for (Task, i64) {
    fn key(&self) -> String {
        format!("{}:{}", self.1, self.0.id)
    }
}

/// The `q` table's statements.
pub(crate) mod db {
    use diesel::prelude::*;

    use super::{Task, Tasks};
    use crate::{db::Page, models::schema::q};

    /// Inserts `task`, stamped `now`.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        task: &Task,
        now: i64,
    ) -> QueryResult<()> {
        diesel::insert_into(q::table)
            .values((task, q::created_at.eq(now), q::updated_at.eq(now)))
            .execute(connection)
            .map(drop)
    }

    /// Replaces the stored `task` whole, stamped `now`; whether it was there.
    pub(crate) fn update(
        connection: &mut SqliteConnection,
        task: &Task,
        now: i64,
    ) -> QueryResult<bool> {
        diesel::update(q::table.find(&task.id))
            .set((task, q::updated_at.eq(now)))
            .execute(connection)
            .map(|rows| rows > 0)
    }

    /// The task `id`, if any.
    pub(crate) fn find_by_id(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> QueryResult<Option<Task>> {
        q::table
            .find(id)
            .select(Task::as_select())
            .first(connection)
            .optional()
    }

    /// Deletes every job of one of `kinds` that succeeded and was last
    /// written before `before` (Unix ms); how many.
    pub(crate) fn delete_succeeded_before(
        connection: &mut SqliteConnection,
        kinds: &[&str],
        before: i64,
    ) -> QueryResult<usize> {
        diesel::delete(
            q::table
                .filter(q::kind.eq_any(kinds))
                .filter(q::status.eq(2))
                .filter(q::updated_at.lt(before)),
        )
        .execute(connection)
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
        use diesel::{
            dsl::sql,
            sql_types::{BigInt, Integer},
        };
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

    /// At most `page.limit` tasks last written, latest first, of `network`,
    /// `kind` and `status` when given, each with its `updated_at`; by
    /// (`updated_at`, `id`), after `page.after` in that order when given. A
    /// job's network is its input's `chain` and `network`, which every job
    /// kind's input carries.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        network: Option<&base::Network>,
        kind: Option<&str>,
        status: Option<i32>,
        page: Page<(i64, String)>,
    ) -> QueryResult<Vec<(Task, i64)>> {
        use diesel::{
            dsl::sql,
            sql_types::{BigInt, Bool, Text},
        };
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
        if let Some(status) = status {
            query = query.filter(q::status.eq(status));
        }
        if let Some((updated_at, id)) = page.after {
            query = query.filter(
                sql::<Bool>("(updated_at, id) < (")
                    .bind::<BigInt, _>(updated_at)
                    .sql(", ")
                    .bind::<Text, _>(id)
                    .sql(")"),
            );
        }
        query
            .order((q::updated_at.desc(), q::id.desc()))
            .limit(page.limit)
            .select((Task::as_select(), q::updated_at))
            .load(connection)
    }

    /// Every unfinished (pending or running) task, by id: what `next`
    /// weighs topics against.
    pub(crate) fn find_unfinished(connection: &mut SqliteConnection) -> QueryResult<Tasks> {
        let tasks = q::table
            .filter(q::status.eq_any([0, 1]))
            .order(q::id)
            .select(Task::as_select())
            .load(connection)?;
        Ok(tasks.into())
    }

    /// Every running task back to pending: what a dead process left.
    pub(crate) fn recover(connection: &mut SqliteConnection, now: i64) -> QueryResult<usize> {
        diesel::update(q::table.filter(q::status.eq(1)))
            .set((q::status.eq(0), q::updated_at.eq(now)))
            .execute(connection)
    }
}
