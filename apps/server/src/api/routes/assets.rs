//! `GET /v1/assets`: every configured asset.

use std::sync::Arc;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::models::Session;

use crate::error::Error;
use crate::{
    api::AppState,
    models::{Asset, NetworkName},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(list))
}

/// One configured asset: its id, where its wallets live, and what they hold.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Listing {
    /// The id every wallet route's `asset` takes within `network`: `native`,
    /// or a token's chain-owned id.
    asset: String,
    /// The chain and network this asset's wallets live on.
    network: NetworkName,
    /// The held asset's metadata.
    metadata: Asset,
}

/// `GET /v1/assets`: every configured asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = AssetList)]
struct ListResponse {
    /// Assets in network, then id, order.
    assets: Vec<Listing>,
}

#[scope::utoipa(
    get,
    path = "/v1/assets",
    operation_id = "list_assets",
    summary = "List every configured asset",
    description = "Every asset of every configured network, with its metadata. This is the \
        only list of assets the wallet routes accept.",
    responses(
        (status = 200, description = "Configured assets", body = ListResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error)
    ),
    tag = "assets"
)]
async fn list(
    session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ListResponse>, Error> {
    scope::require!(session, "com.hashinbox.assets.$read");
    let assets = state.container.assets().into_iter();
    Ok(Json(ListResponse {
        assets: assets.map(Listing::from).collect(),
    }))
}

impl From<base::AssetInfo> for Listing {
    fn from(info: base::AssetInfo) -> Self {
        Self {
            asset: info.asset.id.clone(),
            network: info.asset.network.clone().into(),
            metadata: Asset::from(&info),
        }
    }
}
