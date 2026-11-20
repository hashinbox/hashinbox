//! `GET /v1/stats`: what the store holds across every network (wallets,
//! blocks, transactions, triggers, jobs) and the last day hour by hour,
//! whatever the configuration names.

use std::sync::Arc;

use axum::{Json, extract::State};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{api::AppState, usecase::stats};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

/// Jobs by where they are.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct JobCounts {
    pending: u64,
    running: u64,
    succeeded: u64,
    failed: u64,
}

/// One hour of chain time, across every network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
struct ChainHour {
    /// Its start, Unix seconds.
    start: i64,
    /// Live blocks stamped in it.
    blocks: u64,
    /// Kept transactions mined in those blocks.
    transactions: u64,
}

/// One hour of this server's time.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
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
    /// Blocks kept.
    blocks: u64,
    /// Transactions kept mined.
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
    Ok(Json(stats::read(&state.db).await?.into()))
}

impl From<stats::Stats> for StatsResponse {
    fn from(stats: stats::Stats) -> Self {
        let jobs = stats.jobs;
        Self {
            wallets: stats.wallets,
            addresses: stats.addresses,
            tags: stats.tags,
            blocks: stats.blocks,
            transactions: stats.transactions,
            reverted: stats.reverted,
            pending: stats.pending,
            movements: stats.movements,
            triggers: stats.triggers,
            enabled: stats.enabled,
            jobs: JobCounts {
                pending: jobs.pending,
                running: jobs.running,
                succeeded: jobs.succeeded,
                failed: jobs.failed,
            },
            chain: stats
                .chain
                .into_iter()
                .map(|hour| ChainHour {
                    start: hour.start,
                    blocks: hour.blocks,
                    transactions: hour.transactions,
                })
                .collect(),
            server: stats
                .server
                .into_iter()
                .map(|hour| ServerHour {
                    start: hour.start,
                    succeeded: hour.succeeded,
                    failed: hour.failed,
                    runs: hour.runs,
                })
                .collect(),
        }
    }
}
