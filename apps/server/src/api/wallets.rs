//! `/v1/wallets` and `/v1/wallets/{id}`: generate or watch, list, read and
//! archive wallets.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use indexing::{Birthday, BlockPosition, Holder as _, Watching as _};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};
use wallets::SecretBytes;

use super::{Error, ErrorBody};
use crate::{
    asset::AssetTag,
    models::{Label, Wallet},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(read, archive))
}

/// `POST /v1/wallets` body: generate a wallet for one configured asset, or
/// watch an existing address of it without its key.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = CreateWallet)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// The configured asset tag the wallet holds (`GET /v1/assets`).
    #[schema(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
    /// Address text in any form the asset's chain accepts. Present: watch this
    /// address without its key. Absent: generate a new wallet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    address: Option<String>,
    /// The operator's name for the wallet, 1 to 64 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<String>, min_length = 1, max_length = 64)]
    label: Option<Label>,
    /// Where observation begins: `"fresh"` for an address never used before,
    /// or the native position to inspect from. Absent means `"fresh"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    start: Option<Start>,
}

/// A wallet's birthday on the wire: `"fresh"` or a native position.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(untagged)]
enum Start {
    /// The address has never been used: observe from the next block.
    Fresh(Fresh),
    /// Observe from this native position.
    At(u64),
}

/// The `"fresh"` literal of a `Start`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
enum Fresh {
    /// The address has never been used.
    Fresh,
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    operation_id = "create_wallet",
    summary = "Generate a wallet, or watch an address, for one configured asset",
    description = "Without `address`, generates a wallet, stores it with its key, then \
        watches it from creation. Keys never leave the server. Retrying creates another \
        wallet. With `address`, stores and observes that address from registration without \
        its key: the wallet is watch-only, and sending from it answers 422. `label` names \
        either. `start` is `\"fresh\"` (the default) or the native position observation begins \
        at; a position older than the network's backfill policy allows answers 400. An \
        archived wallet's address is refused: archiving is final.",
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet generated or address watched, and registered \
            for indexing", body = Wallet),
        (status = 400, description = "Malformed body, asset tag, label, address text, or a \
            start the backfill policy refuses", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Asset is not configured", body = ErrorBody),
        (status = 409, description = "The address is already an active or archived wallet, \
            an unobserved row of it holds another key, or the asset's network is being \
            rescanned",
            body = ErrorBody),
        (status = 503, description = "Wallet generation or registration is unavailable",
            body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    // No wallet is created on a network while it is being rescanned.
    let _admitted = state
        .wallets
        .admit(&request.asset)
        .map_err(Error::by_asset)?;
    let asset = state
        .wallets
        .asset(&request.asset)
        .map_err(Error::by_asset)?
        .id;
    // A watched address is the asset's own parse of the text; a generated
    // wallet is a new key of the asset and the wallet it builds.
    let (address, secret, generated) = match &request.address {
        Some(text) => {
            let address = state
                .wallets
                .parse(&request.asset, text)
                .map_err(Error::by_asset)?;
            (address, None, None)
        }
        None => {
            let material = state
                .wallets
                .secret(&request.asset)
                .await
                .map_err(Error::by_asset)?;
            let signer = state
                .wallets
                .from_pk(&request.asset, SecretBytes::new(material.as_bytes()))
                .await
                .map_err(Error::by_asset)?;
            (signer.address().clone(), Some(material), Some(signer))
        }
    };
    // Archiving is final: a wallet that was observed and archived is never
    // observed again, so the store is asked before it would answer that
    // wallet back. A row never observed is an interrupted creation's, and is
    // taken over below.
    let archived = state.registry.wallets().await?.into_iter().any(|wallet| {
        wallet.archived_at().is_some()
            && wallet.birthday().is_some()
            && wallet.address() == &address
            && wallet.asset() == &asset
    });
    if archived {
        return Err(Error::conflict("the address belongs to an archived wallet"));
    }
    // Filed before anything observes it, so the key is durable first: an
    // interrupted creation leaves an archived, never-observed row and no
    // watched address without its key.
    let filed = state
        .registry
        .register(indexing::Registration {
            id: uuid::Uuid::now_v7().to_string(),
            address,
            asset,
            label: request.label.map(String::from),
            secret,
        })
        .await?;
    // The same rule, for a wallet archived since the question above.
    if filed.archived_at().is_some() && filed.birthday().is_some() {
        return Err(Error::conflict("the address belongs to an archived wallet"));
    }
    // The index's admission and backfill policy run before the row is
    // observed; a refusal leaves it archived.
    let birthday = match request.start {
        None | Some(Start::Fresh(Fresh::Fresh)) => Birthday::Fresh,
        Some(Start::At(position)) => Birthday::At(BlockPosition(position)),
    };
    state.index.watch(filed.id(), &filed, birthday).await?;
    // The signer: the one just generated, or an interrupted creation's key.
    let signer = match (generated, filed.secret()) {
        (Some(signer), _) => Some(signer),
        (None, Some(key)) => Some(
            state
                .wallets
                .from_pk(&request.asset, SecretBytes::new(key.as_bytes()))
                .await
                .map_err(Error::by_asset)?,
        ),
        (None, None) => None,
    };
    if let Some(signer) = signer {
        state.signers.insert(filed.id().to_owned(), signer)?;
    }
    let wallet = Wallet::find(state.registry.as_ref(), &state.wallets, filed.id()).await?;
    Ok((StatusCode::CREATED, Json(wallet)))
}

/// Query of `GET /v1/wallets`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    /// Include archived wallets; they are left out by default.
    #[serde(default)]
    #[param(default = false)]
    archived: bool,
}

/// `GET /v1/wallets`: the wallets.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = WalletList)]
struct ListResponse {
    /// Wallets in ID order.
    wallets: Vec<Wallet>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets",
    operation_id = "list_wallets",
    summary = "List the wallets",
    description = "Every active wallet and watched address in wallet ID order; with \
        `archived=true`, the archived ones too.",
    params(ListQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Wallets in ID order", body = ListResponse),
        (status = 400, description = "Malformed query", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The wallet collection is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let wallets = Wallet::all(state.registry.as_ref(), &state.wallets)
        .await?
        .into_iter()
        .filter(|wallet| query.archived || !wallet.archived())
        .collect();
    Ok(Json(ListResponse { wallets }))
}

/// The wallet a route reads or archives.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// The wallet ID a create or watch call returned.
    id: String,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}",
    operation_id = "read_wallet",
    summary = "Read one wallet's public metadata",
    description = "The wallet's asset, network, canonical address, watch-only flag, label and \
        archive time, archived or not; never its key.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's public metadata", body = Wallet),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    Ok(Json(
        Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?,
    ))
}

#[utoipa::path(
    delete,
    path = "/v1/wallets/{id}",
    operation_id = "archive_wallet",
    summary = "Archive a wallet",
    description = "Marks the wallet archived and stops observing its address. Its history and \
        balance stay readable from what was indexed; sending from it answers 409. Archiving \
        again answers the same wallet. Archiving is final: its address cannot be created \
        again.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The archived wallet", body = Wallet),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 409, description = "The wallet's network is being rescanned", body = ErrorBody),
        (status = 503, description = "The wallet could not be archived or unwatched",
            body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn archive(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    let found = Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?;
    // A watch is not removed on a network while it is being rescanned.
    let _admitted = match found.asset().map(|tag| state.wallets.admit(tag)) {
        Ok(Ok(admitted)) => Some(admitted),
        Ok(Err(error)) if error.kind != wallets::WalletErrorKind::Unsupported => {
            return Err(error.into());
        }
        _ => None,
    };
    // The index archives the row under its publication permit, so no pass
    // commits for the wallet afterwards. A wallet of a network this process
    // does not serve has no index to unwatch it: the store archives it.
    state.index.unwatch(&path.id).await?;
    let stored = state.registry.wallet(&path.id).await?;
    if let Some(watch) = stored.and_then(|wallet| wallet.watch()) {
        let network = watch.address.network.clone();
        state.registry.forget(&network, &watch).await?;
    }
    state.signers.remove(&path.id)?;
    Ok(Json(
        Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?,
    ))
}
