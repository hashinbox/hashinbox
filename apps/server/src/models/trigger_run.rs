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

/// A run as a trigger's runs page shows it: when it started, and each send
/// job it queued that is still kept, as it is now, in the order queued.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    pub(crate) started_at: i64,
    pub(crate) jobs: Vec<RunJob>,
}

/// One send job of a [`Run`], as it is now.
#[derive(Clone, Debug)]
pub(crate) struct RunJob {
    pub(crate) id: i64,
    pub(crate) status: tasks::Status,
    pub(crate) reason: Option<String>,
}

/// Its start.
impl crate::db::Keyed for Run {
    fn key(&self) -> String {
        self.started_at.to_string()
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

    use super::{Run, RunJob, TriggerRun};
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

    /// At most `page.limit` runs of `trigger`, latest first, started before
    /// `page.after` when given, each with its send jobs still kept
    /// (`trigger_run_with_jobs`), in one query.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        trigger: &str,
        page: Page<i64>,
    ) -> QueryResult<Vec<Run>> {
        use diesel::sql_types::{BigInt, Integer, Nullable, Text};

        /// A run and one of its jobs (none: it queued none still kept).
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = BigInt)]
            started_at: i64,
            #[diesel(sql_type = Nullable<BigInt>)]
            job: Option<i64>,
            #[diesel(sql_type = Nullable<Integer>)]
            status: Option<i32>,
            #[diesel(sql_type = Nullable<Text>)]
            reason: Option<String>,
        }

        let rows: Vec<Row> = diesel::sql_query(
            r#"SELECT started_at, job, status, reason
               FROM trigger_run_with_jobs
               WHERE run IN (
                SELECT id FROM trigger_run
                WHERE "trigger" = ?1 AND started_at < ?2
                ORDER BY started_at DESC
                LIMIT ?3
               )
               ORDER BY started_at DESC, position"#,
        )
        .bind::<Text, _>(trigger)
        .bind::<BigInt, _>(page.after.unwrap_or(i64::MAX))
        .bind::<BigInt, _>(page.limit)
        .load(connection)?;
        let mut runs: Vec<Run> = Vec::new();
        for row in rows {
            if runs
                .last()
                .is_none_or(|run| run.started_at != row.started_at)
            {
                runs.push(Run {
                    started_at: row.started_at,
                    jobs: Vec::new(),
                });
            }
            let (Some(id), Some(status), Some(run)) = (row.job, row.status, runs.last_mut()) else {
                continue;
            };
            run.jobs.push(RunJob {
                id,
                status: crate::models::task::status_of(status),
                reason: row.reason,
            });
        }
        Ok(runs)
    }
}
