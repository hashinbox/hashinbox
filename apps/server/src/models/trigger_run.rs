//! `trigger_run`: when a trigger's runs started and settled.

use diesel::{prelude::*, sqlite::Sqlite};

use super::schema::trigger_run;

/// One run of a trigger.
#[derive(Clone, Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = trigger_run, check_for_backend(Sqlite))]
pub(crate) struct TriggerRun {
    pub(crate) id: String,
    pub(crate) trigger: String,
    /// Unix ms.
    pub(crate) started_at: i64,
    /// Unix ms; `None`: a send is in flight.
    pub(crate) settled_at: Option<i64>,
}

impl TriggerRun {
    /// A run of `trigger` starting `now`, not settled.
    pub(crate) fn new(trigger: &str, now: i64) -> Self {
        Self {
            id: format!("{trigger}.{now}"),
            trigger: trigger.to_owned(),
            started_at: now,
            settled_at: None,
        }
    }
}

/// The `trigger_run` table's statements.
pub(crate) mod db {
    use diesel::prelude::*;

    use super::TriggerRun;
    use crate::models::schema::trigger_run;

    /// Stores `run`.
    ///
    /// # Errors
    ///
    /// When its trigger is unknown, its trigger already started a run at
    /// the same time, or the write fails.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        run: &TriggerRun,
    ) -> QueryResult<usize> {
        diesel::insert_into(trigger_run::table)
            .values(run)
            .execute(connection)
    }

    /// Settles run `id` at `now`; how many runs settled (0 or 1).
    pub(crate) fn settle(
        connection: &mut SqliteConnection,
        id: &str,
        now: i64,
    ) -> QueryResult<usize> {
        diesel::update(trigger_run::table.find(id))
            .filter(trigger_run::settled_at.is_null())
            .set(trigger_run::settled_at.eq(now))
            .execute(connection)
    }

    /// The latest run of `trigger`.
    pub(crate) fn find_last(
        connection: &mut SqliteConnection,
        trigger: &str,
    ) -> QueryResult<Option<TriggerRun>> {
        trigger_run::table
            .filter(trigger_run::trigger.eq(trigger))
            .order(trigger_run::started_at.desc())
            .select(TriggerRun::as_select())
            .first(connection)
            .optional()
    }
}
