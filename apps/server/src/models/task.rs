//! `q`: one row per queued job for its whole life (`schema/queue.sql`). The
//! statements behind `tasks::Store` (`crate::storage::tasks`).

use diesel::{prelude::*, sqlite::Sqlite};

use super::schema::q;

/// One queued job, as read and written (the stamps are the statements').
#[derive(Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = q, check_for_backend(Sqlite))]
pub(crate) struct Task {
    pub(crate) id: String,
    pub(crate) lane: String,
    pub(crate) kind: String,
    pub(crate) state: i32,
    pub(crate) payload: String,
    pub(crate) run_at: i64,
}

impl Task {
    /// The row of `job`.
    ///
    /// # Errors
    ///
    /// When its run time is above SQLite's integer range.
    pub(crate) fn new(job: &tasks::Job) -> anyhow::Result<Self> {
        Ok(Self {
            id: job.id.to_string(),
            lane: job.lane.clone(),
            kind: job.kind.clone(),
            state: match job.state {
                tasks::State::Pending => 0,
                tasks::State::Running => 1,
                tasks::State::Done => 2,
                tasks::State::Failed => 3,
            },
            payload: job.payload.to_string(),
            run_at: super::int(job.run_at)?,
        })
    }
}

collection! {
    /// Tasks, ordered by id.
    Tasks(Task)
}

/// The `q` table's statements.
pub(crate) mod db {
    use diesel::{prelude::*, upsert::excluded};

    use super::{Task, Tasks};
    use crate::models::schema::q;

    /// Inserts or replaces `task` (payload whole), stamped `now`.
    pub(crate) fn upsert(
        connection: &mut SqliteConnection,
        task: &Task,
        now: i64,
    ) -> QueryResult<()> {
        diesel::insert_into(q::table)
            .values((task, q::created_at.eq(now), q::updated_at.eq(now)))
            .on_conflict(q::id)
            .do_update()
            .set((
                q::lane.eq(excluded(q::lane)),
                q::kind.eq(excluded(q::kind)),
                q::state.eq(excluded(q::state)),
                q::payload.eq(excluded(q::payload)),
                q::run_at.eq(excluded(q::run_at)),
                q::updated_at.eq(excluded(q::updated_at)),
            ))
            .execute(connection)
            .map(drop)
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

    /// Unfinished tasks with `run_at <= now`, by id.
    pub(crate) fn find_due(connection: &mut SqliteConnection, now: i64) -> QueryResult<Tasks> {
        let tasks = q::table
            .filter(q::state.eq_any([0, 1]))
            .filter(q::run_at.le(now))
            .order(q::id)
            .select(Task::as_select())
            .load(connection)?;
        Ok(tasks.into())
    }
}
