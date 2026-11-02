//! `GET /v1/stats`: what the store holds across every network (wallets,
//! blocks, transactions, triggers, jobs) and the last day hour by hour,
//! whatever the configuration names.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{Json, extract::State};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    models::{block, leg, tag, task, transaction, trigger, trigger_run, wallet, wallet_index},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// How many hours a series covers, the current one last.
const HOURS: usize = 24;
const HOUR: i64 = 3_600;

/// Jobs by where they are.
#[derive(Clone, Debug, Default, Serialize, utoipa::ToSchema)]
struct JobCounts {
    pending: u64,
    running: u64,
    succeeded: u64,
    failed: u64,
}

/// One hour of chain time, across every network.
#[derive(Clone, Debug, Default, Serialize, utoipa::ToSchema)]
struct ChainHour {
    /// Its start, Unix seconds.
    start: i64,
    /// Live blocks stamped in it.
    blocks: u64,
    /// Kept transactions mined in those blocks.
    transactions: u64,
}

/// One hour of this server's time.
#[derive(Clone, Debug, Default, Serialize, utoipa::ToSchema)]
struct ServerHour {
    /// Its start, Unix seconds.
    start: i64,
    /// Jobs that succeeded in it.
    succeeded: u64,
    /// Jobs that failed for good in it.
    failed: u64,
    /// Trigger runs started in it.
    runs: u64,
}

/// `GET /v1/stats`.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = Stats)]
struct StatsResponse {
    /// Wallets whose key is kept.
    wallets: u64,
    /// Distinct addresses indexed, keyed or watch-only.
    addresses: u64,
    /// Tags on addresses.
    tags: u64,
    /// Live blocks kept.
    blocks: u64,
    /// Blocks a reorg archived.
    reorged: u64,
    /// Transactions kept mined in live blocks.
    transactions: u64,
    /// Of those, how many reverted.
    reverted: u64,
    /// Transactions kept pending.
    pending: u64,
    /// Legs kept: movements and rewards.
    movements: u64,
    /// Triggers kept.
    triggers: u64,
    /// Of those, how many are enabled.
    enabled: u64,
    /// Jobs by where they are.
    jobs: JobCounts,
    /// The last day of chain time (up to the newest kept block), hour by
    /// hour, oldest first.
    chain: Vec<ChainHour>,
    /// The last day of this server's time, hour by hour, oldest first.
    server: Vec<ServerHour>,
}

/// The `HOURS` hours ending with the one `now` (Unix seconds) falls in: the
/// first one's start, and the hour of `stamp` among them.
fn window(now: i64) -> (i64, impl Fn(i64) -> Option<usize>) {
    let first = (now / HOUR + 1 - HOURS as i64) * HOUR;
    let hour = move |stamp: i64| {
        usize::try_from((stamp - first).div_euclid(HOUR))
            .ok()
            .filter(|hour| *hour < HOURS)
    };
    (first, hour)
}

#[utoipa::path(
    get,
    path = "/v1/stats",
    operation_id = "read_stats",
    summary = "Read what the store holds and its last day",
    description = "Counts across every network, and per hour over the last day: blocks and \
        transactions by chain time (up to the newest kept block), jobs and trigger runs by \
        this server's time.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The stats", body = StatsResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "stats"
)]
async fn read(State(state): State<Arc<AppState>>) -> Result<Json<StatsResponse>, Error> {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::invalid_response(error.to_string()))?
        .as_secs();
    let clock = i64::try_from(clock).map_err(|error| Error::invalid_response(error.to_string()))?;
    let response = state
        .storage
        .read(move |connection| {
            let count = |value: i64| u64::try_from(value).unwrap_or_default();
            let (blocks, reorged) = block::db::count(connection)?;
            let (transactions, reverted, pending) = transaction::db::count(connection)?;
            let (triggers, enabled) = trigger::db::count(connection)?;
            let mut jobs = JobCounts::default();
            for (status, n) in task::db::count_by_status(connection)? {
                let n = count(n);
                match status {
                    0 => jobs.pending = n,
                    1 => jobs.running = n,
                    2 => jobs.succeeded = n,
                    _ => jobs.failed = n,
                }
            }

            // Chain time ends at the newest kept block: a skewed clock or a
            // lagging indexer leaves no empty hours.
            let newest = block::db::find_newest_stamp(connection)?.unwrap_or(clock);
            let (first, hour) = window(newest);
            let mut chain: Vec<ChainHour> = (0..HOURS)
                .map(|i| ChainHour {
                    start: first + i as i64 * HOUR,
                    ..ChainHour::default()
                })
                .collect();
            for stamp in block::db::find_stamps_since(connection, first)? {
                if let Some(i) = hour(stamp) {
                    chain[i].blocks += 1;
                }
            }
            for stamp in transaction::db::find_stamps_since(connection, first)? {
                if let Some(i) = hour(stamp) {
                    chain[i].transactions += 1;
                }
            }

            let (first, hour) = window(clock);
            let mut server: Vec<ServerHour> = (0..HOURS)
                .map(|i| ServerHour {
                    start: first + i as i64 * HOUR,
                    ..ServerHour::default()
                })
                .collect();
            for (status, at) in task::db::find_finished_since(connection, first * 1_000)? {
                if let Some(i) = hour(at / 1_000) {
                    match status {
                        2 => server[i].succeeded += 1,
                        _ => server[i].failed += 1,
                    }
                }
            }
            for at in trigger_run::db::find_starts_since(connection, first * 1_000)? {
                if let Some(i) = hour(at / 1_000) {
                    server[i].runs += 1;
                }
            }

            Ok(StatsResponse {
                wallets: count(wallet::db::count(connection)?),
                addresses: wallet_index::db::count_addresses(connection)? as u64,
                tags: count(tag::db::count(connection)?),
                blocks: count(blocks),
                reorged: count(reorged),
                transactions: count(transactions),
                reverted: count(reverted),
                pending: count(pending),
                movements: count(leg::db::count(connection)?),
                triggers: count(triggers),
                enabled: count(enabled),
                jobs,
                chain,
                server,
            })
        })
        .await?;
    Ok(Json(response))
}
