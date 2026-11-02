//! `trigger_run`: when a trigger's runs started, and the jobs each queued.

use diesel::{prelude::*, sqlite::Sqlite};

use super::{Json, schema::trigger_run};

/// One run of a trigger.
#[derive(Clone, Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = trigger_run, check_for_backend(Sqlite))]
pub(crate) struct TriggerRun {
    pub(crate) id: String,
    pub(crate) trigger: String,
    /// Unix ms.
    pub(crate) started_at: i64,
    /// The ids of the send jobs it queued.
    pub(crate) jobs: Json<Vec<String>>,
}

impl TriggerRun {
    /// A run of `trigger` starting `now`.
    pub(crate) fn new(trigger: &str, now: i64) -> Self {
        Self {
            id: format!("{trigger}.{now}"),
            trigger: trigger.to_owned(),
            started_at: now,
            jobs: Json(Vec::new()),
        }
    }
}

/// The `trigger_run` table's statements.
pub(crate) mod db {
    use diesel::prelude::*;

    use super::TriggerRun;
    use crate::models::schema::trigger_run;

    /// When (Unix ms) every run started at or after `since` started.
    pub(crate) fn find_starts_since(
        connection: &mut SqliteConnection,
        since: i64,
    ) -> QueryResult<Vec<i64>> {
        trigger_run::table
            .filter(trigger_run::started_at.ge(since))
            .select(trigger_run::started_at)
            .load(connection)
    }

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

    /// The latest `limit` runs of `trigger`, latest first.
    pub(crate) fn find_recent(
        connection: &mut SqliteConnection,
        trigger: &str,
        limit: i64,
    ) -> QueryResult<Vec<TriggerRun>> {
        trigger_run::table
            .filter(trigger_run::trigger.eq(trigger))
            .order(trigger_run::started_at.desc())
            .limit(limit)
            .select(TriggerRun::as_select())
            .load(connection)
    }
}
