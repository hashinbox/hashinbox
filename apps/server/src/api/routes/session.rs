//! `/v1/session`: the session is the JWT (design 096). A login with a
//! password or the recovery passphrase (with its two-factor code) or a
//! key's bearer secret issues one; the
//! caller's is read back decoded; logging out everywhere refuses every
//! session its issuer issued until now.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::AppState,
    error::Error,
    models::{Caller, Client, User, UserKey, session::Amr, user_key::KeySecret},
    usecase::sessions::{self, Credential, Issued, Login, Packed},
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(create, read, delete))
}

/// `POST /v1/session` body: `email` and `password` or `passphrase` (and
/// `totp`) log in; with `Authorization: Bearer his/…`, the key's session,
/// for `sub`.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateSessionRequest)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// A login: the user's email.
    #[serde(default)]
    #[schema(nullable = false)]
    email: Option<String>,
    /// A login: the user's password; or `passphrase`, not both.
    #[serde(default)]
    #[schema(nullable = false)]
    password: Option<String>,
    /// A login: the recovery passphrase (12 words, any spacing or case),
    /// for any user; or `password`, not both.
    #[serde(default)]
    #[schema(nullable = false)]
    passphrase: Option<String>,
    /// A login: the 6-digit two-factor code, when it is on.
    #[serde(default)]
    #[schema(nullable = false)]
    totp: Option<String>,
    /// A key's session: who it is for (a customer); logged, grants
    /// nothing.
    #[serde(default)]
    #[schema(nullable = false)]
    sub: Option<String>,
    /// Fewer scopes than the issuer's, packed
    /// (`{"com.hashinbox": ["wallets[x].$read"]}`); the issuer's when
    /// absent.
    #[serde(default)]
    #[schema(nullable = false, value_type = Option<std::collections::BTreeMap<String, Vec<String>>>)]
    scopes: Option<Packed>,
    /// Seconds it lasts, 1 to 86400; 43200 when absent.
    #[serde(default)]
    #[schema(nullable = false, minimum = 1, maximum = 86400)]
    ttl: Option<i64>,
}

/// A session issued.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = IssuedSession)]
pub(super) struct CreateResponse {
    /// The JWT: send it as `Authorization: Bearer …`.
    token: String,
    /// Unix seconds it expires.
    expires_at: i64,
    /// What it may do, packed.
    #[schema(value_type = std::collections::BTreeMap<String, Vec<String>>)]
    scopes: Packed,
    /// The logged-in user (a login only).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    user: Option<User>,
}

impl CreateResponse {
    /// The answer for `issued`, logged in as `user`.
    pub(super) fn new(issued: Issued, user: Option<User>) -> Self {
        Self {
            token: issued.token,
            expires_at: issued.session.expires_at,
            scopes: issued.session.scopes.pack(),
            user,
        }
    }
}

/// The key's secret `headers` send as the bearer, if they do.
fn bearer_secret(headers: &HeaderMap) -> Option<KeySecret> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (_, token) = header.split_once(' ')?;
    KeySecret::parse(token.trim())
}

#[utoipa::path(
    post,
    path = "/v1/session",
    operation_id = "session_create",
    summary = "Log in, or sign a key's session",
    description = "With `email` and `password` or `passphrase` (the recovery passphrase, any \
        user's email; and `totp` once two-factor is on) and no `Authorization`: a session of \
        the user, its scopes or fewer, its `amr` `pwd` or `passphrase` (and `otp`). A wrong \
        email, password, passphrase or code answers the same 401; the rest right without the \
        code answers 401 `totp_required`. With `Authorization: Bearer his/…`: a session of \
        the key, signed here, for a caller that cannot sign (`sub`, `scopes` covered by the \
        key's).",
    request_body = CreateRequest,
    responses(
        (status = 201, description = "Issued", body = CreateResponse),
        (status = 400, description = "Malformed body, scopes or ttl; neither or both of \
            password and passphrase", body = Error),
        (status = 401, description = "Wrong email, password, passphrase or code \
            (`wrong_credentials`), or `totp_required`", body = Error),
        (status = 403, description = "Scopes wider than the issuer's", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn create(
    caller: Option<Caller>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateResponse>), Error> {
    let Json(request) = request?;
    if let Some(key) = caller.and_then(|caller| caller.key) {
        let secret = bearer_secret(&headers).ok_or_else(|| {
            Error::invalid_argument("a key's session is signed here only for its bearer secret")
        })?;
        let issued = sessions::issue_for_key(
            &key,
            &secret,
            request.sub,
            request.scopes.as_ref(),
            request.ttl,
        )?;
        return Ok((StatusCode::CREATED, Json(CreateResponse::new(issued, None))));
    }
    let credential = match (request.password, request.passphrase) {
        (Some(password), None) => Credential::Password(password),
        (None, Some(passphrase)) => Credential::Passphrase(passphrase),
        _ => {
            return Err(Error::invalid_argument(
                "a login needs a password or a passphrase, exactly one",
            ));
        }
    };
    let Some(email) = request.email else {
        return Err(Error::invalid_argument("a login needs an email"));
    };
    let login = Login {
        email,
        credential,
        totp: request.totp,
        scopes: request.scopes,
        ttl: request.ttl,
    };
    let (issued, user) = sessions::login(&state.db, login).await?;
    Ok((
        StatusCode::CREATED,
        Json(CreateResponse::new(issued, Some(user))),
    ))
}

// Named apart from the error `Kind`: two schemas of one name collapse into one in the
// OpenAPI document.
/// Who issued a session.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum Issuer {
    /// The user, at login.
    User,
    /// A key: its secret, or a session it signed.
    Key,
}

/// The caller's session, decoded and checked.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = SessionRead)]
struct ReadResponse {
    /// Who issued it.
    kind: Issuer,
    /// Its user.
    user: User,
    /// Its key, when a key issued it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    key: Option<UserKey>,
    /// Who it is for; always present, null: not said.
    #[schema(required)]
    sub: Option<String>,
    /// What it may do, packed.
    #[schema(value_type = std::collections::BTreeMap<String, Vec<String>>)]
    scopes: Packed,
    /// Unix seconds.
    issued_at: i64,
    /// Unix seconds; a key's bearer secret: now.
    expires_at: i64,
    /// How its login proved who it is (`pwd` or `passphrase`, then `otp`);
    /// empty for a key's.
    amr: Vec<Amr>,
}

#[utoipa::path(
    get,
    path = "/v1/session",
    operation_id = "session_get",
    summary = "Read the caller's session",
    description = "The session the bearer token is, decoded and checked: its issuer, user, \
        key, scopes, times and `amr`.",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The session", body = ReadResponse),
        (status = 401, description = "Missing or invalid bearer token", body = Error)
    ),
    tag = "auth"
)]
async fn read(caller: Caller) -> Json<ReadResponse> {
    let kind = match caller.session.client {
        Client::User(_) => Issuer::User,
        Client::Key(_) => Issuer::Key,
    };
    Json(ReadResponse {
        kind,
        user: caller.user,
        key: caller.key,
        sub: caller.session.sub,
        scopes: caller.session.scopes.pack(),
        issued_at: caller.session.issued_at,
        expires_at: caller.session.expires_at,
        amr: caller.session.amr,
    })
}

/// `DELETE /v1/session` query.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct DeleteQuery {
    /// Every session the issuer (the user, or the key) issued until now:
    /// refused.
    #[serde(default)]
    all: bool,
}

#[utoipa::path(
    delete,
    path = "/v1/session",
    operation_id = "session_delete",
    summary = "Log out",
    description = "Alone, nothing: the client forgets its token. With `all=true`, every \
        session the caller's issuer (its user, or its key) issued until now answers 401, \
        this one too.",
    params(DeleteQuery),
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Logged out"),
        (status = 400, description = "Malformed query", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn delete(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
) -> Result<StatusCode, Error> {
    let Query(query) = query?;
    if query.all {
        sessions::invalidate_issuer(&state.db, &caller.user, &caller.session).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
