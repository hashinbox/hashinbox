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
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::models::Caller;
use crate::models::Session;

use crate::{
    api::{AppState, IndexParams, create_status},
    db::Paged as _,
    error::Error,
    models::{Wallet, wallet::WalletId},
    usecase::wallets::{self, NewWallet},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(read))
}

/// `POST /v1/wallets` body: a wallet in one network, created without
/// indexes. With `secret`, its key is imported and kept; else with
/// `address`, the address is watched without a key; with neither, a new key
/// is generated and kept.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateWalletRequest)]
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
    /// Your own reference for the wallet (e.g. your customer id), 1 to 128
    /// characters once trimmed; not unique.
    #[serde(default)]
    #[schema(nullable = false, min_length = 1, max_length = 128)]
    external_id: Option<String>,
}

impl CreateRequest {
    /// The wallet asked for, its `external_id` trimmed.
    ///
    /// # Errors
    ///
    /// When `external_id` is empty once trimmed or longer than 128
    /// characters.
    fn wallet(self) -> Result<NewWallet, Error> {
        let external_id = self.external_id.as_deref().map(str::trim);
        if external_id.is_some_and(|text| !(1..=128).contains(&text.chars().count())) {
            return Err(Error::invalid_argument(
                "external_id must be 1 to 128 characters once trimmed",
            ));
        }
        Ok(NewWallet {
            external_id: external_id.map(str::to_owned),
            network: base::Network::new(base::Chain::new(self.chain), self.network),
            secret: self.secret,
            address: self.address,
        })
    }
}

#[scope::utoipa(
    post,
    path = "/v1/wallets",
    operation_id = "wallets_create",
    summary = "Import a wallet's key, watch an address, or generate a wallet, in one configured network",
    description = "With `secret`, imports a raw private key and keeps it; with `address` too, \
        the key must derive that address. With `address` alone, watches it without its key. \
        With neither, generates a key and keeps it; retrying makes another wallet. The wallet \
        is created without indexes (`POST /v1/wallets/{id}/index`). An address already kept \
        answers its wallet as is (200), except that importing the key of a watch-only wallet \
        keeps the key, and an `external_id` is kept on a wallet that has none; another \
        `external_id` than the kept one is refused (409); its tags and indexes stay.",
    request_body = CreateRequest,
    responses(
        (status = 201, description = "Wallet created", body = Wallet),
        (status = 200, description = "The address was already kept: its wallet", body = Wallet),
        (status = 400, description = "Malformed body, address, key or external ID, or a key \
            that derives another address", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "Network is not configured", body = Error),
        (status = 409, description = "The address is kept with another external ID",
            body = Error),
        (status = 503, description = "Generation or the store is unavailable",
            body = Error)
    ),
    tag = "wallets"
)]
async fn create(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    scope::require!(caller, "com.hashinbox.wallets");
    let Json(request) = request?;
    let new = request.wallet()?;
    let (created, wallet) = wallets::create(&state.container, &state.db, new).await?;
    Ok((create_status(created), Json(wallet)))
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

#[scope::utoipa(
    get,
    path = "/v1/wallets",
    operation_id = "wallets_list",
    summary = "List the wallets",
    description = "Keyed wallets and watched addresses, a page in wallet ID order, each with \
        its tags and its indexes: their balances (exact atomic units), progress and legs. A \
        caller granted only some wallets reads each by its ID.",
    params(IndexParams),
    responses(
        (status = 200, description = "Wallets in ID order", body = ListResponse),
        (status = 400, description = "Malformed query or limit", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The wallet store is unavailable", body = Error)
    ),
    tag = "wallets"
)]
async fn list(
    session: Session,
    State(state): State<Arc<AppState>>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    scope::require!(session, "com.hashinbox.wallets:read");
    let Query(index) = index?;
    let page = index.page("wallet list", |cursor| Some(cursor.to_owned()))?;
    let limit = page.limit;
    let wallets = wallets::list_page(&state.db, page).await?;
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

#[scope::utoipa(
    get,
    path = "/v1/wallets/{id}",
    operation_id = "wallet_get",
    summary = "Read one wallet's public metadata",
    description = "The wallet's network, canonical address, whether it is keyed, the assets \
        it is indexed in and how far each is; never its key.",
    params(WalletPath),
    responses(
        (status = 200, description = "The wallet's public metadata", body = Wallet),
        (status = 400, description = "Malformed wallet ID", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "Wallet is unknown", body = Error),
        (status = 503, description = "The wallet store is unavailable", body = Error)
    ),
    tag = "wallets"
)]
async fn read(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    scope::require!(session, "com.hashinbox.wallets[{id}]:read", id = path.id);
    let id = WalletId::parse(&path.id)?;
    Ok(Json(wallets::read(&state.container, &state.db, &id).await?))
}
