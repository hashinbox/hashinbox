//! `q`: one row per queued job for its whole life (`schema/queue.sql`). The
//! statements behind `tasks::Store` (`crate::storage::tasks`).

use diesel::{prelude::*, sqlite::Sqlite, upsert::excluded};

use super::schema::q;

/// One `q` row, as read and written (the stamps are the statements').
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
    /// Inserts or replaces the row (payload whole), stamped `now`.
    pub(crate) fn set(&self, connection: &mut SqliteConnection, now: i64) -> QueryResult<()> {
        diesel::insert_into(q::table)
            .values((self, q::created_at.eq(now), q::updated_at.eq(now)))
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

    /// The row of `id`, if any.
    pub(crate) fn get(connection: &mut SqliteConnection, id: &str) -> QueryResult<Option<Self>> {
        q::table
            .find(id)
            .select(Self::as_select())
            .first(connection)
            .optional()
    }

    /// Unfinished rows with `run_at <= now`, by id.
    pub(crate) fn next(connection: &mut SqliteConnection, now: i64) -> QueryResult<Vec<Self>> {
        q::table
            .filter(q::state.eq_any([0, 1]))
            .filter(q::run_at.le(now))
            .order(q::id)
            .select(Self::as_select())
            .load(connection)
    }
}
