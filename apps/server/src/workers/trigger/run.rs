//! A trigger's runs settle once every job finished and what it sent is
//! confirmed.

use base::Network;
use diesel::SqliteConnection;
use tasks::{Job, Jobs, Status};

use crate::{
    models::{TriggerRun, block, task, transaction, transaction::Transaction, trigger_run},
    workers::broadcast::Progress,
};

/// Settles every run of `network`'s triggers that is done: every job
/// finished, and every transaction a done job sent is in a live block at or
/// below the last kept block (a failed job is done, whatever it sent is
/// indexed anyway). The ids of the triggers whose runs settled.
///
/// # Errors
///
/// When a read or write fails, or a job's state does not decode.
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
    let jobs: Jobs = task::db::find_by_ids(connection, &run.jobs.0)?
        .into_iter()
        .map(Job::try_from)
        .collect::<anyhow::Result<Vec<_>>>()?
        .into();
    if !jobs.finished() {
        return Ok(false);
    }
    // A failed job is definitively over; a done one waits for what it sent.
    for job in jobs.iter().filter(|job| job.status == Status::Done) {
        let progress: Progress = job
            .state
            .clone()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        for txid in &progress.ids[..progress.sent.min(progress.ids.len())] {
            let id = Transaction::id(network, txid)?;
            if !transaction::db::is_confirmed(connection, &id, confirmed)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
