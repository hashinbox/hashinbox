use crate::settings;
use std::sync::Arc;

use super::{
    Payments,
    contract::Network,
    error::{Error, ErrorBody},
};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn routes() -> OpenApiRouter<super::Application> {
    OpenApiRouter::new().routes(routes!(change))
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Action {
    Pause,
    Resume,
    Reindex,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Command {
    network: Network,
    action: Action,
}

#[utoipa::path(put, path = "/v1/indexing", tag = "indexing",
    operation_id = "control_indexing", request_body = Command,
    summary = "Pause, resume or reindex one network",
    description = "Persists requested indexing state. Reindex pauses operations, \
        purges indexed facts, preserves wallet keys and birthdays, then scans again. \
        Poll chain settings for progress.",
    security(("bearerAuth" = [])), responses(
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 202, description = "Indexing request persisted"),
        (status = 204, description = "Indexing control applied"),
        (status = 404, description = "Network not configured", body = ErrorBody),
        (status = 409, description = "Supervisor stopped", body = ErrorBody),
        (status = 400, description = "Invalid command", body = ErrorBody),
        (status = 503, description = "Control unavailable or still pausing", body = ErrorBody)))]
async fn change(
    State(payments): State<Arc<Payments>>,
    State(application): State<super::Application>,
    body: Result<Json<Command>, JsonRejection>,
) -> Result<StatusCode, Error> {
    let Json(command) = body.map_err(Error::invalid_json)?;
    let network = base::Network::new(
        base::ChainName::new(command.network.chain),
        command.network.name,
    );
    if let Some(runtime) = application.runtime {
        let action = match command.action {
            Action::Pause => settings::Action::Pause,
            Action::Resume => settings::Action::Resume,
            Action::Reindex => settings::Action::Reindex,
        };
        runtime
            .settings
            .command(&network, action)
            .await
            .map_err(|_| {
                Error::invalid_request(
                    "indexing command rejected; check network and current reindex",
                )
            })?;
        return Ok(StatusCode::ACCEPTED);
    }
    let control = payments
        .control
        .as_ref()
        .ok_or_else(|| Error::indexed("index control unavailable"))?;
    match command.action {
        Action::Pause => {
            tokio::time::timeout(std::time::Duration::from_secs(30), control.pause(&network))
                .await
                .map_err(|_| {
                    Error::indexed("pause requested; inspect network status for completion")
                })??
        }
        Action::Resume => control.resume(&network)?,
        Action::Reindex => return Err(Error::indexed("runtime recovery unavailable")),
    }
    Ok(StatusCode::NO_CONTENT)
}
