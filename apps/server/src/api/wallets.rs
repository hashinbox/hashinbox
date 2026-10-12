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
use indexing::{Birthday, BlockPosition, Watching as _};
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
/// watch an existing address in it without its key. A wallet is an address:
/// watching another asset of an address that already is a wallet adds that
/// asset to it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = CreateWallet)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// The configured asset tag to observe the wallet in (`GET /v1/assets`).
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
    summary = "Generate a wallet, or watch an address, in one configured asset",
    description = "Without `address`, generates a wallet, stores it with its key, then \
        watches it in `asset` from creation. Keys never leave the server. Retrying creates \
        another wallet. With `address`, observes that address in `asset` from registration: \
        a new address becomes a watch-only wallet, and sending from it answers 422; an \
        address that already is a wallet gains `asset`. `label` names either. `start` is \
        `\"fresh\"` (the default) or the native position observation of `asset` begins at; \
        a position older than the network's backfill policy allows answers 400. An archived \
        wallet's address is refused: archiving is final.",
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet generated or address watched, and registered \
            for indexing", body = Wallet),
        (status = 400, description = "Malformed body, asset tag, label, address text, or a \
            start the backfill policy refuses", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Asset is not configured", body = ErrorBody),
        (status = 409, description = "The address is an archived wallet, is already watched \
            in this asset from another start, or the asset's network is being rescanned",
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
        .payments
        .wallets
        .admit(&request.asset)
        .map_err(Error::by_asset)?;
    let info = state
        .payments
        .wallets
        .asset(&request.asset)
        .map_err(Error::by_asset)?;
    // A watched address is the asset's own parse of the text; a generated
    // wallet is a new key of the asset and the wallet it builds.
    let (address, key, generated) = match &request.address {
        Some(text) => {
            let address = state
                .payments
                .wallets
                .parse(&request.asset, text)
                .map_err(Error::by_asset)?;
            (address, None, None)
        }
        None => {
            let material = state
                .payments
                .wallets
                .secret(&request.asset)
                .await
                .map_err(Error::by_asset)?;
            let signer = state
                .payments
                .wallets
                .from_pk(&request.asset, SecretBytes::new(material.as_bytes()))
                .await
                .map_err(Error::by_asset)?;
            let key = wallets::PrivateKey {
                format: info.key_format,
                secret: material,
            };
            (signer.address().clone(), Some(key), Some(signer))
        }
    };
    // Archiving is final: an archived wallet's address is never observed
    // again. An active wallet of the address is the one this adds an asset
    // to; otherwise a new one is filed.
    let registry = Wallet::registry(&state.payments)?;
    let held = registry
        .wallets()
        .await?
        .into_iter()
        .find(|wallet| wallet.address() == &address);
    if held
        .as_ref()
        .is_some_and(|wallet| wallet.archived_at().is_some())
    {
        return Err(Error::conflict("the address belongs to an archived wallet"));
    }
    let id = held.map_or_else(
        || uuid::Uuid::now_v7().to_string(),
        |wallet| wallet.id().to_owned(),
    );
    // Filed before anything observes it, so the key is durable first: an
    // interrupted creation leaves an unobserved wallet, never a watched
    // address without its key.
    let filed = registry
        .register(wallets::Registration {
            id,
            address: address.clone(),
            label: request.label.map(String::from),
            key,
        })
        .await?;
    // The same rule, for a wallet archived since the question above.
    if filed.archived_at().is_some() {
        return Err(Error::conflict("the address belongs to an archived wallet"));
    }
    // The index's admission and backfill policy run before the asset is
    // observed; a refusal leaves the wallet filed and unobserved in it.
    let birthday = match request.start {
        None | Some(Start::Fresh(Fresh::Fresh)) => Birthday::Fresh,
        Some(Start::At(position)) => Birthday::At(BlockPosition(position)),
    };
    let subject = indexing::Subject {
        address,
        asset: info.id,
    };
    state.payments.watch(filed.id(), &subject, birthday).await?;
    // The signer: the one just generated, or one from the stored key.
    let signer = match (generated, filed.key()) {
        (Some(signer), _) => Some(signer),
        (None, Some(key)) if key.format == info.key_format => Some(
            state
                .payments
                .wallets
                .from_pk(&request.asset, SecretBytes::new(key.secret.as_bytes()))
                .await
                .map_err(Error::by_asset)?,
        ),
        (None, _) => None,
    };
    if let Some(signer) = signer {
        state.payments.wallets.insert_signer(
            filed.id().to_owned(),
            request.asset.clone(),
            signer,
        )?;
    }
    let wallet = Wallet::find(&state.payments, filed.id()).await?;
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
    let wallets = Wallet::all(&state.payments)
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
    description = "The wallet's network, canonical address, watch-only flag, label, archive \
        time and observed assets, archived or not; never its key.",
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
    Ok(Json(Wallet::find(&state.payments, &path.id).await?))
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
    let found = Wallet::find(&state.payments, &path.id).await?;
    // A watch is not removed on a network while it is being rescanned; the
    // pause is per network, so any configured asset of it admits.
    let tag = state
        .payments
        .wallets
        .assets()
        .into_iter()
        .find(|info| info.network == found.address().network)
        .map(|info| info.tag);
    let _admitted = match tag.map(|tag| state.payments.wallets.admit(&tag)) {
        Some(Ok(admitted)) => Some(admitted),
        Some(Err(error)) if error.kind != wallets::WalletErrorKind::Unsupported => {
            return Err(error.into());
        }
        _ => None,
    };
    // The index removes every watch of the wallet under its publication
    // permit, so no pass commits for it afterwards; then the wallet row is
    // archived, which also keeps a network this process does not serve from
    // scanning it.
    state.payments.index.unwatch(&path.id).await?;
    Wallet::registry(&state.payments)?.archive(&path.id).await?;
    state.payments.wallets.remove_signers(&path.id)?;
    Ok(Json(Wallet::find(&state.payments, &path.id).await?))
}
