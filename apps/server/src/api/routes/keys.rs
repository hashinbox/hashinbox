//! `/v1/user/keys`: the caller's user's keys. Create (scopes covered by the
//! creator's, the secret shown once), list (never a secret), revoke, and
//! rotate (every session the key signed until now refused).

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

use crate::{
    api::{AppState, IndexParams},
    db::Paged as _,
    error::Error,
    models::{Caller, UserKey},
    usecase::{
        keys::{self, NewKey},
        sessions::Packed,
    },
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(revoke))
        .routes(routes!(rotate))
}

/// `GET /v1/user/keys`: a page of keys.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = KeyList)]
struct ListResponse {
    /// The caller's user's keys, in id order.
    keys: Vec<UserKey>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[scope::utoipa(
    get,
    path = "/v1/user/keys",
    operation_id = "keys_list",
    summary = "List the keys",
    description = "The caller's user's keys, revoked ones too, a page in id order; never a \
        secret.",
    params(IndexParams),
    responses(
        (status = 200, description = "Keys in id order", body = ListResponse),
        (status = 400, description = "Malformed query or limit", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn list(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    index: Result<Query<IndexParams>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    scope::require!(caller, "com.hashinbox.users.keys:read");
    let Query(index) = index?;
    let page = index.page("key list", |cursor| Some(cursor.to_owned()))?;
    let limit = page.limit;
    let keys = keys::list_page(&state.db, &caller.user.id, page).await?;
    Ok(Json(ListResponse {
        next_cursor: keys.cursor(limit),
        keys,
    }))
}

/// `POST /v1/user/keys` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateKeyRequest)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// What it is for, 1 to 128 characters: `shop backend`.
    name: String,
    /// Its scopes, packed, covered by the caller's; the caller's when
    /// absent.
    #[serde(default)]
    #[schema(nullable = false, value_type = Option<std::collections::BTreeMap<String, Vec<String>>>)]
    scopes: Option<Packed>,
    /// Unix ms it stops working; never when absent.
    #[serde(default)]
    #[schema(nullable = false)]
    expires_at: Option<i64>,
}

/// A key created, and its secret, shown this once.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = CreatedKey)]
struct CreateResponse {
    /// The key.
    key: UserKey,
    /// `his/…`: its secret, never shown again. A bearer token as it is, and
    /// the HS256 key, as it is, of the sessions the key signs (`kid` and
    /// `iss`: the key's id).
    secret: String,
}

#[scope::utoipa(
    post,
    path = "/v1/user/keys",
    operation_id = "keys_create",
    summary = "Create a key",
    description = "A key of the caller's user, its scopes covered by the caller's. Its secret \
        is in this answer only.",
    request_body = CreateRequest,
    responses(
        (status = 201, description = "Created", body = CreateResponse),
        (status = 400, description = "Malformed body, name or scopes", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope, or scopes wider than the caller's",
            body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn create(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateResponse>), Error> {
    scope::require!(caller, "com.hashinbox.users.keys");
    let Json(request) = request?;
    let name = request.name.trim();
    if !(1..=128).contains(&name.chars().count()) {
        return Err(Error::invalid_argument("name must be 1 to 128 characters"));
    }
    let new = NewKey {
        name: name.to_owned(),
        scopes: request.scopes,
        expires_at: request.expires_at,
    };
    let (user, within) = (&caller.user, &caller.session.scopes);
    let (key, secret) = keys::create(&state.db, user, within, new).await?;
    Ok((StatusCode::CREATED, Json(CreateResponse { key, secret })))
}

/// The key a route changes.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct KeyPath {
    /// Its id without `hip/` (or `hip%2F…`).
    id: String,
}

impl KeyPath {
    /// The key's id: `hip/{id}`.
    fn key_id(&self) -> String {
        let id = self.id.strip_prefix("hip/").unwrap_or(&self.id);
        format!("hip/{id}")
    }
}

#[scope::utoipa(
    delete,
    path = "/v1/user/keys/{id}",
    operation_id = "key_revoke",
    summary = "Revoke a key",
    description = "The key (its secret) and every session it signed answer 401 for good. `id` \
        is the key's id without `hip/`.",
    params(KeyPath),
    responses(
        (status = 204, description = "Revoked"),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "No such key of the caller's user, or revoked already",
            body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn revoke(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    Path(path): Path<KeyPath>,
) -> Result<StatusCode, Error> {
    scope::require!(caller, "com.hashinbox.users.keys");
    keys::revoke(&state.db, &caller.user, &path.key_id()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[scope::utoipa(
    post,
    path = "/v1/user/keys/{id}/rotate",
    operation_id = "key_rotate",
    summary = "Rotate a key's sessions",
    description = "Every session the key signed until now answers 401; the key itself keeps \
        working. `id` is the key's id without `hip/`.",
    params(KeyPath),
    responses(
        (status = 200, description = "Rotated", body = UserKey),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 404, description = "No such key of the caller's user", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn rotate(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    Path(path): Path<KeyPath>,
) -> Result<Json<UserKey>, Error> {
    scope::require!(caller, "com.hashinbox.users.keys");
    let key = keys::rotate(&state.db, &caller.user, &path.key_id()).await?;
    Ok(Json(key))
}
