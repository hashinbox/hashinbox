//! The SQLite driver over a caller-supplied pool. It never applies schema:
//! the caller runs `schema/queue.sql` first (the server's `migrate`).
//!
//! Each write is one `BEGIN IMMEDIATE` transaction, so claims from several
//! workers serialize on the file's write lock and one task has one owner.

use sqlx::{Row as _, SqliteConnection, SqlitePool};

use super::{Store, StoreError, StoreFuture, retry_at, validate};
use crate::task::{Claimed, Id, NewTask, Outcome, Settlement, Status, TaskResult};

/// The queue on one SQLite pool.
#[derive(Clone)]
pub struct Sqlite {
    pool: SqlitePool,
}

/// Each lane's oldest live task.
const HEADS: &str =
    "SELECT q.id FROM q WHERE q.id = (SELECT MIN(h.id) FROM q h WHERE h.lane = q.lane)";

impl Sqlite {
    /// The queue over `pool`, whose file already holds `schema/queue.sql`.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn backend(error: sqlx::Error) -> StoreError {
    StoreError::Backend(error.to_string())
}

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn uint(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Moves `id` from `q` to `q_result`; `error` falls back to the last retry reason.
async fn finish(
    connection: &mut SqliteConnection,
    id: &str,
    outcome: Outcome,
    output: Option<Vec<u8>>,
    error: Option<String>,
    now: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO q_result (task_id, outcome, output, error, attempts, finished_at)
         SELECT id, ?2, ?3, COALESCE(?4, error), attempts, ?5 FROM q WHERE id = ?1",
    )
    .bind(id)
    .bind(outcome.code())
    .bind(output)
    .bind(error)
    .bind(int(now))
    .execute(&mut *connection)
    .await?;
    sqlx::query("DELETE FROM q WHERE id = ?1")
        .bind(id)
        .execute(&mut *connection)
        .await?;
    Ok(())
}

impl Store for Sqlite {
    fn enqueue<'a>(&'a self, task: NewTask, now: u64) -> StoreFuture<'a, Id> {
        Box::pin(async move {
            validate(&task)?;
            let id = Id::new();
            let backoff = u64::try_from(task.policy.backoff.as_millis()).unwrap_or(u64::MAX);
            sqlx::query(
                "INSERT INTO q (id, lane, kind, payload, run_at, deadline, attempts, max_attempts,
                 backoff, state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, 0, ?9)",
            )
            .bind(id.as_str())
            .bind(&task.lane)
            .bind(&task.kind)
            .bind(&task.payload)
            .bind(int(task.run_at))
            .bind(task.deadline.map(int))
            .bind(i64::from(task.policy.max_attempts))
            .bind(int(backoff))
            .bind(int(now))
            .execute(&self.pool)
            .await
            .map_err(backend)?;
            Ok(id)
        })
    }

    fn claim<'a>(
        &'a self,
        owner: &'a str,
        lease_until: u64,
        now: u64,
    ) -> StoreFuture<'a, Option<Claimed>> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(backend)?;
            let lapsed = format!("{HEADS} AND (q.state = 0 OR q.lease_until < ?1)");
            let ended = sqlx::query(&format!(
                "SELECT id, deadline IS NOT NULL AND deadline <= ?1 AS expired FROM q WHERE id IN ({lapsed})
                 AND ((deadline IS NOT NULL AND deadline <= ?1) OR (state = 1 AND attempts >= max_attempts))"
            ))
            .bind(int(now))
            .fetch_all(&mut *transaction)
            .await
            .map_err(backend)?;
            for row in ended {
                let id: String = row.get("id");
                let (outcome, error) = if row.get::<bool, _>("expired") {
                    (Outcome::Expired, None)
                } else {
                    (Outcome::Failed, Some("lease expired".to_owned()))
                };
                finish(&mut transaction, &id, outcome, None, error, now)
                    .await
                    .map_err(backend)?;
            }
            let claimed = sqlx::query(&format!(
                "UPDATE q SET state = 1, owner = ?2, lease_until = ?3, attempts = attempts + 1
                 WHERE id = (SELECT id FROM q WHERE id IN ({lapsed}) AND run_at <= ?1 ORDER BY run_at, id LIMIT 1)
                 RETURNING id, lane, kind, payload, attempts"
            ))
            .bind(int(now))
            .bind(owner)
            .bind(int(lease_until))
            .fetch_optional(&mut *transaction)
            .await
            .map_err(backend)?;
            transaction.commit().await.map_err(backend)?;
            Ok(claimed.map(|row| Claimed {
                id: Id::from(row.get::<String, _>("id")),
                lane: row.get("lane"),
                kind: row.get("kind"),
                payload: row.get("payload"),
                attempt: u32::try_from(row.get::<i64, _>("attempts")).unwrap_or(u32::MAX),
            }))
        })
    }

    fn settle<'a>(
        &'a self,
        id: &'a Id,
        owner: &'a str,
        settlement: Settlement,
        now: u64,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(backend)?;
            let held = sqlx::query("SELECT attempts, max_attempts, backoff FROM q WHERE id = ?1 AND state = 1 AND owner = ?2")
                .bind(id.as_str())
                .bind(owner)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(backend)?
                .ok_or(StoreError::Lost)?;
            let attempts: i64 = held.get("attempts");
            let exhausted = attempts >= held.get::<i64, _>("max_attempts");
            match settlement {
                Settlement::Finish { outcome, output, error } => {
                    finish(&mut transaction, id.as_str(), outcome, output, error, now).await
                }
                Settlement::Retry { error } if exhausted => {
                    finish(&mut transaction, id.as_str(), Outcome::Failed, None, Some(error), now).await
                }
                Settlement::Retry { error } => {
                    let attempts = u32::try_from(attempts).unwrap_or(u32::MAX);
                    sqlx::query("UPDATE q SET state = 0, owner = NULL, lease_until = NULL, run_at = ?2, error = ?3 WHERE id = ?1")
                        .bind(id.as_str())
                        .bind(int(retry_at(now, uint(held.get("backoff")), attempts)))
                        .bind(error)
                        .execute(&mut *transaction)
                        .await
                        .map(drop)
                }
            }
            .map_err(backend)?;
            transaction.commit().await.map_err(backend)
        })
    }

    fn cancel<'a>(&'a self, id: &'a Id, now: u64) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(backend)?;
            let pending = sqlx::query("SELECT 1 FROM q WHERE id = ?1 AND state = 0")
                .bind(id.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(backend)?
                .is_some();
            if pending {
                finish(
                    &mut transaction,
                    id.as_str(),
                    Outcome::Cancelled,
                    None,
                    None,
                    now,
                )
                .await
                .map_err(backend)?;
            }
            transaction.commit().await.map_err(backend)?;
            Ok(pending)
        })
    }

    fn status<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Status>> {
        Box::pin(async move {
            let live = sqlx::query("SELECT state FROM q WHERE id = ?1")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
            if let Some(row) = live {
                return Ok(Some(if row.get::<i64, _>("state") == 0 {
                    Status::Pending
                } else {
                    Status::Running
                }));
            }
            let Some(row) = sqlx::query("SELECT outcome, output, error, attempts, finished_at FROM q_result WHERE task_id = ?1")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?
            else {
                return Ok(None);
            };
            let outcome = Outcome::from_code(row.get("outcome"))
                .ok_or_else(|| StoreError::Backend("unknown outcome code".into()))?;
            Ok(Some(Status::Finished(TaskResult {
                task: id.clone(),
                outcome,
                output: row.get("output"),
                error: row.get("error"),
                attempts: u32::try_from(row.get::<i64, _>("attempts")).unwrap_or(u32::MAX),
                finished_at: uint(row.get("finished_at")),
            })))
        })
    }
}
