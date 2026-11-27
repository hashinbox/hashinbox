//! `/v1/recovery`: the recovery passphrase (design 096), optional, 12
//! BIP39 words the root creates, replaces or removes; shown once. They sign
//! in as any user (`POST /v1/session` with `passphrase`), two-factor still
//! asked. Every route needs `recovery`, reading it too: only the
//! root holds it.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{api::AppState, error::Error, models::Caller, usecase::recovery};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read, create, remove))
}

/// `GET /v1/recovery`: whether a passphrase is set, never its words.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = Recovery)]
struct ReadResponse {
    /// Whether a passphrase is set.
    set: bool,
    /// When it was created (Unix ms); absent when none is set.
    created_at: Option<i64>,
}

#[scope::utoipa(
    get,
    path = "/v1/recovery",
    operation_id = "recovery_get",
    summary = "Read whether the recovery passphrase is set",
    description = "Whether a passphrase is set and when it was created; never its words or \
        hash. Needs `recovery`, as the other recovery routes do.",
    responses(
        (status = 200, description = "Set or not", body = ReadResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn read(
    caller: Caller,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReadResponse>, Error> {
    scope::require!(caller, "com.hashinbox.recovery");
    let created_at = recovery::set_at(&state.db).await?;
    let set = created_at.is_some();
    Ok(Json(ReadResponse { set, created_at }))
}

/// `POST /v1/recovery`: the words, shown this once.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = RecoveryCreated)]
struct CreateResponse {
    /// 12 BIP39 English words, space-separated. Only their hash is kept:
    /// they are never shown again.
    passphrase: String,
}

#[scope::utoipa(
    post,
    path = "/v1/recovery",
    operation_id = "recovery_create",
    summary = "Create or replace the recovery passphrase",
    description = "12 new BIP39 words, answered this once, in place of any passphrase set; \
        only their argon2id hash is kept. Replacing ends every session the old words made \
        (`session_expired`). They sign in as any user (`POST /v1/session` with `passphrase`), \
        two-factor still asked: whoever holds them is every user without two-factor.",
    responses(
        (status = 201, description = "Created: the words, shown once", body = CreateResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn create(
    caller: Caller,
    State(state): State<Arc<AppState>>,
) -> Result<(StatusCode, Json<CreateResponse>), Error> {
    scope::require!(caller, "com.hashinbox.recovery");
    let mnemonic = recovery::create(&state.db, &caller.user.id).await?;
    let passphrase = mnemonic.to_string();
    Ok((StatusCode::CREATED, Json(CreateResponse { passphrase })))
}

#[scope::utoipa(
    delete,
    path = "/v1/recovery",
    operation_id = "recovery_delete",
    summary = "Remove the recovery passphrase",
    description = "Passphrase logins are off (refused as wrong credentials) and every session \
        the words made ends (`session_expired`). 204 when none is set too.",
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn remove(caller: Caller, State(state): State<Arc<AppState>>) -> Result<StatusCode, Error> {
    scope::require!(caller, "com.hashinbox.recovery");
    recovery::remove(&state.db, &caller.user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}
