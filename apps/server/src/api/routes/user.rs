//! `/v1/user`: the logged-in user (always the caller, like `/v1/session`),
//! its password (`PUT`) and its two-factor (TOTP). Any session reads it;
//! only the user's own sessions change it, never a key.

use std::sync::Arc;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::session::CreateResponse as IssuedSession;
use crate::{
    api::AppState,
    error::Error,
    models::{Caller, User, UserKey},
    usecase::users,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(read, update))
        .routes(routes!(start_two_factor, stop_two_factor))
        .routes(routes!(confirm_two_factor))
}

/// The caller's own user, or 403 for a key: a password or a two-factor is
/// the user's alone.
fn own_user(caller: &Caller) -> Result<&User, Error> {
    if caller.key.is_some() {
        return Err(Error::permission_denied("a key may not change its user"));
    }
    Ok(&caller.user)
}

/// A user's two-factor.
#[derive(Serialize, utoipa::ToSchema)]
struct TwoFactor {
    /// Unix ms it was started.
    created_at: i64,
    /// Unix ms its first code was entered; always present, null: pending,
    /// not on yet.
    #[schema(required)]
    confirmed_at: Option<i64>,
}

/// `GET /v1/user`.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = UserRead)]
struct ReadResponse {
    /// The caller's user.
    user: User,
    /// The calling key, when a key calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    key: Option<UserKey>,
    /// Its two-factor; always present, null: off.
    #[schema(required)]
    two_factor: Option<TwoFactor>,
}

#[utoipa::path(
    get,
    path = "/v1/user",
    operation_id = "read_user",
    summary = "Read the logged-in user",
    description = "The caller's user, its scopes and its two-factor; the calling key too, \
        when a key calls.",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The user", body = ReadResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn read(
    caller: Caller,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReadResponse>, Error> {
    let two_factor = users::two_factor_of(&state.db, &caller.user.id).await?;
    Ok(Json(ReadResponse {
        user: caller.user,
        key: caller.key,
        two_factor: two_factor.map(|row| TwoFactor {
            created_at: row.created_at,
            confirmed_at: row.confirmed_at,
        }),
    }))
}

/// `PUT /v1/user` body: what changes.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = UpdateOwnUser)]
#[serde(deny_unknown_fields)]
struct UpdateRequest {
    /// The new password, 8 characters at least; the current one is not
    /// asked.
    #[schema(min_length = 8)]
    password: String,
}

#[utoipa::path(
    put,
    path = "/v1/user",
    operation_id = "update_own_user",
    summary = "Change the logged-in user's password",
    description = "Sets a new password, checked as at signup; the current one is not asked, so \
        any session of the user (a passphrase login too) may. Every other session it and its \
        keys issued ends (401); the answer is a fresh session, the caller's scopes and `amr`.",
    request_body = UpdateRequest,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Changed: a fresh session", body = IssuedSession),
        (status = 400, description = "Malformed body, or a password too short", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "A key calls", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn update(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    request: Result<Json<UpdateRequest>, JsonRejection>,
) -> Result<Json<IssuedSession>, Error> {
    let user = own_user(&caller)?;
    let Json(request) = request?;
    let issued = users::set_password(&state.db, user, &caller.session, request.password).await?;
    Ok(Json(IssuedSession::new(issued, Some(caller.user.clone()))))
}

/// `POST /v1/user/2fa`.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = TwoFactorStarted)]
struct StartResponse {
    /// The `otpauth://` URI for an authenticator app (its QR code).
    uri: String,
}

#[utoipa::path(
    post,
    path = "/v1/user/2fa",
    operation_id = "start_two_factor",
    summary = "Start two-factor",
    description = "A new TOTP secret, pending until its first code is confirmed \
        (`POST /v1/user/2fa/confirm`); starting again replaces a pending one.",
    security(("bearer" = [])),
    responses(
        (status = 201, description = "Started", body = StartResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "A key calls", body = Error),
        (status = 422, description = "Two-factor is on already", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn start_two_factor(
    caller: Caller,
    State(state): State<Arc<AppState>>,
) -> Result<(StatusCode, Json<StartResponse>), Error> {
    let user = own_user(&caller)?;
    let uri = users::start_two_factor(&state.db, user).await?;
    Ok((StatusCode::CREATED, Json(StartResponse { uri })))
}

/// A two-factor code.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = TwoFactorCode)]
#[serde(deny_unknown_fields)]
struct CodeRequest {
    /// The 6 digits the authenticator app shows.
    code: String,
}

#[utoipa::path(
    post,
    path = "/v1/user/2fa/confirm",
    operation_id = "confirm_two_factor",
    summary = "Confirm two-factor",
    description = "Its first valid code turns two-factor on: a login needs a code from now.",
    request_body = CodeRequest,
    security(("bearer" = [])),
    responses(
        (status = 204, description = "On"),
        (status = 400, description = "Malformed body", body = Error),
        (status = 401, description = "Missing or invalid bearer token, or a wrong code",
            body = Error),
        (status = 403, description = "A key calls", body = Error),
        (status = 404, description = "Two-factor was not started", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn confirm_two_factor(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    request: Result<Json<CodeRequest>, JsonRejection>,
) -> Result<StatusCode, Error> {
    let user = own_user(&caller)?;
    let Json(request) = request?;
    users::confirm_two_factor(&state.db, user, request.code).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/v1/user/2fa",
    operation_id = "stop_two_factor",
    summary = "Turn two-factor off",
    description = "Needs a valid code; logs the user out everywhere.",
    request_body = CodeRequest,
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Off"),
        (status = 400, description = "Malformed body", body = Error),
        (status = 401, description = "Missing or invalid bearer token, or a wrong code",
            body = Error),
        (status = 403, description = "A key calls", body = Error),
        (status = 404, description = "Two-factor was not started", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn stop_two_factor(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    request: Result<Json<CodeRequest>, JsonRejection>,
) -> Result<StatusCode, Error> {
    let user = own_user(&caller)?;
    let Json(request) = request?;
    users::stop_two_factor(&state.db, user, request.code).await?;
    Ok(StatusCode::NO_CONTENT)
}
