//! The job queue as kept: every job, where it is, why it failed, what it was
//! given and what its worker has done so far.

use super::workers::broadcast;
use crate::error::Error;
use crate::{
    db::{Db, Keyed, Page},
    models::task,
};

/// One job of the queue: the job, when it was queued and last written (Unix
/// ms), and for a broadcast, the transaction it recorded (txid) and whether
/// it was sent (none and false for other kinds).
pub(crate) struct Job {
    pub(crate) job: tasks::Job,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) txid: Option<String>,
    pub(crate) accepted: bool,
}

impl Job {
    /// Row `id` of the queue, with when it was queued and last written.
    fn new((id, row, created_at, updated_at): (i64, task::Task, i64, i64)) -> anyhow::Result<Self> {
        let job = row.into_job(id)?;
        let (txid, accepted) = broadcast::progress(job.state.as_ref());
        Ok(Self {
            job,
            created_at,
            updated_at,
            txid,
            accepted,
        })
    }
}

/// `"{updated_at}:{id}"`.
impl Keyed for Job {
    fn key(&self) -> String {
        format!("{}:{}", self.updated_at, self.job.id.0)
    }
}

/// A page of the jobs last written, latest first: of `network`, `kind` and
/// `status` when given (`None`: every status but idle).
///
/// # Errors
///
/// The read, or a job that does not read back.
pub(crate) async fn list_page(
    db: &Db,
    network: Option<base::Network>,
    kind: Option<String>,
    status: Option<tasks::Status>,
    page: Page<(i64, i64)>,
) -> Result<Vec<Job>, Error> {
    let rows = db
        .read(move |connection| {
            let status = status.map(task::status);
            let rows =
                task::db::find_page(connection, network.as_ref(), kind.as_deref(), status, page)?;
            Ok(rows)
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(Job::new)
        .collect::<anyhow::Result<_>>()?)
}

/// Job `id`.
///
/// # Errors
///
/// Not found for an unknown id; the read, or a job that does not read back.
pub(crate) async fn read(db: &Db, id: &str) -> Result<Job, Error> {
    let Ok(id) = id.parse() else {
        return Err(Error::not_found("job does not exist"));
    };
    let row = db
        .read(move |connection| Ok(task::db::find_by_id(connection, id)?))
        .await?
        .ok_or_else(|| Error::not_found("job does not exist"))?;
    Ok(Job::new(row)?)
}
