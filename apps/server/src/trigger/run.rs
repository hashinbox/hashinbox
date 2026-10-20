//! A trigger's runs: started with their send jobs in one transaction,
//! settled once every job finished and what it sent is confirmed.

use base::Network;
use diesel::SqliteConnection;
use tasks::Job;

use crate::{
    models::{Task, TriggerRun, block, task, transaction, transaction::Transaction, trigger_run},
    workers::send::Progress,
};

/// Keeps `run` and its send `jobs` together: both or neither.
///
/// # Errors
///
/// When a write fails or a job's run time does not fit.
pub(crate) fn start(
    connection: &mut SqliteConnection,
    run: &TriggerRun,
    jobs: &[Job],
    now: i64,
) -> anyhow::Result<()> {
    trigger_run::db::insert(connection, run)?;
    for job in jobs {
        task::db::upsert(connection, &Task::new(job)?, now)?;
    }
    Ok(())
}

/// Settles every run of `network`'s triggers that is done: every job
/// finished, and every transaction a done job sent is in a live block at or
/// below the last kept block (a failed job is done, whatever it sent is
/// indexed anyway). The ids of the triggers whose runs settled.
///
/// # Errors
///
/// When a read or write fails, or a job's payload does not decode.
pub(crate) fn settle(
    connection: &mut SqliteConnection,
    network: &Network,
    now: i64,
) -> anyhow::Result<Vec<String>> {
    let Some(last) = block::db::find_last(connection, network)? else {
        return Ok(Vec::new());
    };
    let mut settled = Vec::new();
    for run in trigger_run::db::find_unsettled(connection, network)? {
        if is_done(connection, network, &run, last.position)?
            && trigger_run::db::settle(connection, &run.id, now)? == 1
        {
            settled.push(run.trigger);
        }
    }
    Ok(settled)
}

/// Whether every job of `run` finished and every transaction a done job
/// sent is confirmed at `confirmed`.
fn is_done(
    connection: &mut SqliteConnection,
    network: &Network,
    run: &TriggerRun,
    confirmed: i64,
) -> anyhow::Result<bool> {
    for id in &run.jobs.0 {
        let Some(job) = task::db::find_by_id(connection, id)? else {
            continue;
        };
        match job.state {
            // Failed: definitively over.
            3 => continue,
            2 => {}
            _ => return Ok(false),
        }
        let payload: serde_json::Value = serde_json::from_str(&job.payload)?;
        let progress: Progress = payload
            .get("progress")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        for txid in &progress.ids {
            let id = Transaction::id(network, txid)?;
            if !transaction::db::is_confirmed(connection, &id, confirmed)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
