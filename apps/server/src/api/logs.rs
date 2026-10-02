use std::sync::Arc;

use super::{Payments, error::Error};
use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn routes() -> OpenApiRouter<super::Application> {
    OpenApiRouter::new().routes(routes!(read))
}

/// Exact context filters, combined with AND. Cursors are process-local.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    chain: Option<String>,
    network: Option<String>,
    node_id: Option<String>,
    component: Option<String>,
    #[serde(default)]
    #[param(default = 0)]
    after: u64,
    #[serde(default = "Selection::limit")]
    #[param(minimum = 1, maximum = 1000, default = 100)]
    limit: usize,
}

impl Selection {
    const fn limit() -> usize {
        100
    }
    fn filter(&self) -> logging::Filter {
        let fields = [
            ("chain", &self.chain),
            ("network", &self.network),
            ("node_id", &self.node_id),
            ("component", &self.component),
        ];
        logging::Filter(
            fields
                .into_iter()
                .filter_map(|(key, value)| {
                    value.as_ref().map(|value| (key.to_owned(), value.clone()))
                })
                .collect(),
        )
    }
}

/// One retained event, with public context and no raw request or provider payload.
#[derive(Serialize, ToSchema)]
pub(super) struct Event {
    sequence: u64,
    timestamp: u64,
    level: String,
    event: String,
    message: String,
    fields: BTreeMap<String, String>,
    truncated: bool,
}

impl From<logging::Entry> for Event {
    fn from(entry: logging::Entry) -> Self {
        Self {
            sequence: entry.sequence,
            timestamp: entry.timestamp,
            level: entry.level.to_string().to_lowercase(),
            event: entry.event,
            message: entry.message,
            fields: entry.fields,
            truncated: entry.truncated,
        }
    }
}

/// Polling result. A gap means some records were lost; logs are not an audit ledger.
#[derive(Serialize, ToSchema)]
pub(super) struct Events {
    session: String,
    entries: Vec<Event>,
    next: u64,
    gap: bool,
}

#[utoipa::path(get, path = "/v1/logs", tag = "logs", operation_id = "read_logs", params(Selection),
    summary = "Read filtered retained logs",
    description = "Returns bounded in-memory events matching every supplied filter. \
        Use the returned cursor for polling; a changed session requires restarting the cursor.",
    security(("bearerAuth" = [])),
    responses((status = 401, description = "Unauthorized", body = super::error::ErrorBody),
              (status = 200, description = "Retained events", body = Events),
              (status = 400, description = "Invalid query", body = super::error::ErrorBody)))]
async fn read(
    State(payments): State<Arc<Payments>>,
    query: Result<Query<Selection>, QueryRejection>,
) -> Result<Json<Events>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    if !(1..=1000).contains(&query.limit) {
        return Err(Error::invalid_request("limit must be 1..=1000"));
    }
    let page = payments
        .logs
        .page(query.after, query.limit, &query.filter());
    Ok(Json(Events {
        session: page.session,
        entries: page.entries.into_iter().map(Into::into).collect(),
        next: page.next,
        gap: page.gap,
    }))
}
