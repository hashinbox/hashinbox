//! `/v1/wallets` and `/v1/wallets/{id}`: generate, import or watch, list and
//! read wallets (`wallet_indexes` indexes them). A wallet is an address:
//! keyed (its key kept in its `wallet` row) or watch-only, with its indexes,
//! their balances, progress and legs. The answer is the model
//! (`models::Wallet`) as read at the indexers' tips.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use base::Chain;
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, IndexParams, wallet_address};
use crate::{
    api::AppState,
    db::Paged as _,
    models::{self, Wallet},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(read))
}

/// `POST /v1/wallets` body: a wallet in one network, created without
/// indexes. With `secret`, its key is imported and kept; else with
/// `address`, the address is watched without a key; with neither, a new key
/// is generated and kept.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateWallet)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// Chain name, as `GET /v1/assets` lists it.
    chain: String,
    /// Network name, as `GET /v1/assets` lists it.
    network: String,
    /// A raw private key in the chain's import format: Ethereum hex, Solana
    /// Base58 secret or keypair, Bitcoin WIF.
    #[serde(default)]
    #[schema(nullable = false)]
    secret: Option<String>,
    /// Address text in any form the chain accepts; with `secret`, the
    /// address the key must derive.
    #[serde(default)]
    #[schema(nullable = false)]
    address: Option<String>,
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    operation_id = "create_wallet",
    summary = "Import a wallet's key, watch an address, or generate a wallet, in one configured network",
    description = "With `secret`, imports a raw private key and keeps it; with `address` too, \
        the key must derive that address. With `address` alone, watches it without its key. \
        With neither, generates a key and keeps it; retrying makes another wallet. The wallet \
        is created without indexes (`POST /v1/wallets/{id}/index`). An address already kept \
        answers its wallet as is (200), except that importing the key of a watch-only wallet \
        keeps the key; its tags and indexes stay.",
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet created", body = Wallet),
        (status = 200, description = "The address was already kept: its wallet", body = Wallet),
        (status = 400, description = "Malformed body, address or key, or a key that derives \
            another address", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network is not configured", body = ErrorBody),
        (status = 503, description = "Generation or the store is unavailable",
            body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let network = base::Network::new(Chain::new(request.chain), request.network);
    let factory = &state.payments.service(&network)?.wallets.factory;
    let given = request
        .address
        .map(|text| factory.parse_address(&text))
        .transpose()?;
    let row = if let Some(secret) = request.secret {
        let wallet = factory.from_pk_text(&secret).await?;
        if let Some(given) = given.filter(|given| given != wallet.address()) {
            return Err(Error::invalid_request(format!(
                "the key derives {}, not {}",
                wallet.address().value,
                given.value
            )));
        }
        models::Wallet::try_from(&*wallet)
    } else if let Some(address) = given {
        models::Wallet::from_address(&address)
    } else {
        let wallet = factory.from_pk(factory.generate().await?).await?;
        models::Wallet::try_from(&*wallet)
    }
    .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    let (created, wallet) = state
        .db
        .write(move |connection| {
            let created = models::wallet::db::insert(connection, &row)?;
            let wallet = models::wallet::db::find(connection, &row.id)?;
            Ok((created, wallet))
        })
        .await?;
    let wallet = wallet.ok_or_else(|| Error::not_found("wallet is unknown"))?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(wallet)))
}

/// `GET /v1/wallets`: a page of wallets.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = WalletList)]
struct ListResponse {
    /// Wallets in ID order.
    wallets: Vec<Wallet>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets",
    operation_id = "list_wallets",
    summary = "List the wallets",
    description = "Keyed wallets and watched addresses, a page in wallet ID order, each with \
        its tags and its indexes: their balances (exact atomic units), progress and legs.",
    params(IndexParams),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Wallets in ID order", body = ListResponse),
        (status = 400, description = "Malformed query or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The wallet store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(index) = index.map_err(Error::invalid_query)?;
    let page = index.page("wallet list", |cursor| Some(cursor.to_owned()))?;
    let limit = page.limit;
    let wallets = state
        .db
        .read(move |connection| models::wallet::db::find_page(connection, page))
        .await?;
    Ok(Json(ListResponse {
        next_cursor: wallets.cursor(limit),
        wallets,
    }))
}

/// The wallet a route reads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
pub(super) struct WalletPath {
    /// `{chain}.{network}.{address}`, as a create call returned it.
    pub(super) id: String,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}",
    operation_id = "read_wallet",
    summary = "Read one wallet's public metadata",
    description = "The wallet's network, canonical address, whether it is keyed, the assets \
        it is indexed in and how far each is; never its key.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's public metadata", body = Wallet),
        (status = 400, description = "Malformed wallet ID", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 503, description = "The wallet store is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    let address = wallet_address(&state.payments, &path.id)?;
    let id = models::Wallet::id(&address)
        .map_err(|error| Error::invalid_request(format!("{error:#}")))?;
    state
        .db
        .read(move |connection| models::wallet::db::find(connection, &id))
        .await?
        .map(Json)
        .ok_or_else(|| Error::not_found("wallet is unknown"))
}
