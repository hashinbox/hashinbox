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

/// Its start.
impl crate::db::Keyed for TriggerRun {
    fn key(&self) -> String {
        self.started_at.to_string()
    }
}

/// The `trigger_run` table's statements.
pub(crate) mod db {
    use diesel::prelude::*;

    use super::TriggerRun;
    use crate::{
        db::Page,
        models::schema::{trigger_run, triggers},
    };

    /// How many runs started in each hour from `since` (Unix seconds) on,
    /// as (hour, count); hour 0 starts at `since`.
    pub(crate) fn count_by_hour(
        connection: &mut SqliteConnection,
        since: i64,
    ) -> QueryResult<Vec<(i64, i64)>> {
        use diesel::{dsl::sql, sql_types::BigInt};
        trigger_run::table
            .filter(trigger_run::started_at.ge(since * 1_000))
            .group_by(sql::<BigInt>("hour"))
            .select((
                sql::<BigInt>("(started_at / 1000 - ")
                    .bind::<BigInt, _>(since)
                    .sql(") / 3600 AS hour"),
                diesel::dsl::count_star(),
            ))
            .load(connection)
    }

    /// Stores `run` as its trigger's latest.
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
            .execute(connection)?;
        diesel::update(triggers::table.find(&run.trigger))
            .set(triggers::last_run.eq(&run.id))
            .execute(connection)
    }

    /// At most `page.limit` runs of `trigger` started before `page.after`
    /// when given, latest first.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        trigger: &str,
        page: Page<i64>,
    ) -> QueryResult<Vec<TriggerRun>> {
        trigger_run::table
            .filter(trigger_run::trigger.eq(trigger))
            .filter(trigger_run::started_at.lt(page.after.unwrap_or(i64::MAX)))
            .order(trigger_run::started_at.desc())
            .limit(page.limit)
            .select(TriggerRun::as_select())
            .load(connection)
    }
}
