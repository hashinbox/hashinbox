use std::sync::Arc;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::ErrorBody;
use crate::{
    models::{Asset, Network},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// One configured asset: its tag, where its wallets live, and what they hold.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Listing {
    /// The tag every wallet route takes (`GET /v1/assets` is the whole list).
    #[schema(pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: String,
    /// The chain and network this asset's wallets live on.
    network: Network,
    /// The held asset's metadata.
    metadata: Asset,
}

/// `GET /v1/assets`: every configured asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = AssetList)]
struct ListResponse {
    /// Assets in tag order.
    assets: Vec<Listing>,
}

#[utoipa::path(
    get,
    path = "/v1/assets",
    operation_id = "list_assets",
    summary = "List every configured asset",
    description = "Every asset tag registered at startup, with its network and metadata. This \
        is the only list of tags the wallet routes accept.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Configured assets in tag order", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    ),
    tag = "assets"
)]
async fn list(State(state): State<Arc<AppState>>) -> Json<ListResponse> {
    // Startup refuses a factory whose chain declared no unit for its asset,
    // so every registered tag has one and is listed.
    let assets = state
        .wallets
        .assets()
        .into_iter()
        .filter_map(|info| {
            let unit = state.units.of(&info.network, &info.id)?;
            Some(Listing {
                asset: info.tag.to_string(),
                network: info.network.into(),
                metadata: Asset::new(&info.id, unit),
            })
        })
        .collect();
    Json(ListResponse { assets })
}
