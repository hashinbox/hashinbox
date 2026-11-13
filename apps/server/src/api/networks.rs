//! `GET /v1/networks`: every stored network (its row, assets, nodes and
//! index), and the node's tip asked now.

use std::{sync::Arc, time::Duration};

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use tasks::Worker as _;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    api::AppState,
    models::{Network, network, task},
    usecase::workers::index,
};

/// Longest the list waits for one network's node to tell its tip.
const LIVE: Duration = Duration::from_secs(2);

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// Every stored network.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = NetworkList)]
struct ListResponse {
    /// Networks in chain, then name, order.
    networks: Vec<Network>,
}

#[utoipa::path(
    get,
    path = "/v1/networks",
    operation_id = "list_networks",
    summary = "List every network, its assets, its RPC nodes, its indexing and its live tip",
    description = "Every stored network: its row, its assets, its nodes (each URL with what \
        the running server knows of it: last tip, failure streak, calls) and \
        its `index` row (null: not indexed), and why its index job last failed \
        (`index_job_reason`). `tip_live` asks every running network's node at \
        once, each for at most 2 s; one that fails keeps the stored `tip` and says why in \
        `tip_error`.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Every network", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The networks could not be read", body = ErrorBody)
    ),
    tag = "networks"
)]
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ListResponse>, Error> {
    let rows = state
        .db
        .read(|connection| {
            let mut rows = network::db::find_all(connection)?;
            for row in &mut rows {
                let target = index::Target {
                    chain: row.chain.clone(),
                    network: row.network.clone(),
                };
                let topic = index::Index::topic(&target).map(|topic| topic.to_string());
                let job = match topic {
                    Some(topic) => task::db::find_last(connection, index::KIND, &topic)?,
                    None => None,
                };
                row.index_job_reason = job.and_then(|job| job.reason);
            }
            Ok(rows)
        })
        .await?;
    let networks = rows.into_iter().map(|mut row| {
        let state = &state;
        async move {
            if let Ok(transport) = state.container.transport(&row.key()) {
                transport.clients().states(&mut row.endpoints).await;
            }
            let service = match state.container.service(&row.key()) {
                Ok(service) => service,
                Err(error) => {
                    row.tip_error = Some(format!("{error:#}"));
                    return row;
                }
            };
            let live = match &service.dated {
                None => Err("this network cannot read its tip".to_owned()),
                Some(dated) => match tokio::time::timeout(LIVE, dated.tips()).await {
                    Err(_) => Err("the node did not answer in time".to_owned()),
                    Ok(tips) => tips.map(|tips| tips.produced).map_err(|e| format!("{e:#}")),
                },
            };
            match live {
                Ok(tip) => row.tip_live = Some(tip),
                Err(error) => row.tip_error = Some(error),
            }
            row
        }
    });
    let networks = futures_util::future::join_all(networks.collect::<Vec<_>>()).await;
    Ok(Json(ListResponse { networks }))
}

/// The network a route acts on.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct NetworkPath {
    /// Chain name, as `GET /v1/networks` lists it.
    chain: String,
    /// Network name, as `GET /v1/networks` lists it.
    network: String,
}

impl NetworkPath {
    /// The network, when configured; a 404 when not.
    pub(super) fn network(
        &self,
        container: &base::Container<crate::usecase::Transport>,
    ) -> Result<base::Network, Error> {
        let network = base::Network::new(base::Chain::new(&self.chain), self.network.clone());
        container.service(&network)?;
        Ok(network)
    }
}
