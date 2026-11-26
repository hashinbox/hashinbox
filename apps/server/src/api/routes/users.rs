//! `/v1/users`: list, create (the root user on a fresh install, needing no
//! `Authorization`; else scopes covered by the creator's), change scopes,
//! disable.

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
    models::{Caller, User},
    usecase::{
        sessions::{Packed, bearer_required},
        users::{self, NewUser, UserChange},
    },
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(update))
}

/// `GET /v1/users`: a page of users.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = UserList)]
struct ListResponse {
    /// Users in id (creation) order.
    users: Vec<User>,
    /// Pass as `cursor` for the next page; null on the last page. Always
    /// present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[scope::utoipa(
    get,
    path = "/v1/users",
    operation_id = "users_list",
    summary = "List the users",
    description = "Every user, a page in id order; never a password or a secret.",
    params(IndexParams),
    responses(
        (status = 200, description = "Users in id order", body = ListResponse),
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
    scope::require!(caller, "com.hashinbox.users.$read");
    let Query(index) = index?;
    let page = index.page("user list", |cursor| Some(cursor.to_owned()))?;
    let limit = page.limit;
    let users = users::list_page(&state.db, page).await?;
    Ok(Json(ListResponse {
        next_cursor: users.cursor(limit),
        users,
    }))
}

/// `POST /v1/users` body.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateUserRequest)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// Its login, unique.
    email: String,
    /// Its name, 1 to 128 characters.
    name: String,
    /// Its password, 8 characters at least; absent: no dashboard login
    /// (the root user needs one).
    #[serde(default)]
    #[schema(nullable = false, min_length = 8)]
    password: Option<String>,
    /// Its scopes, packed, covered by the creator's; none when absent.
    /// The root user holds every scope.
    #[serde(default)]
    #[schema(nullable = false, value_type = Option<std::collections::BTreeMap<String, Vec<String>>>)]
    scopes: Option<Packed>,
}

impl CreateRequest {
    /// The user asked for, checked: an email with `@`, a name, a password
    /// of 8 characters at least.
    fn user(self) -> Result<NewUser, Error> {
        NewUser::new(&self.email, &self.name, self.password, self.scopes)
    }
}

/// A user created; the root user logged in at once.
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = CreatedUser)]
struct CreateResponse {
    /// The user.
    user: User,
    /// The root user's session (a fresh install only): send it as
    /// `Authorization: Bearer …`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    token: Option<String>,
    /// Unix seconds that session expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    expires_at: Option<i64>,
}

#[scope::utoipa(
    post,
    path = "/v1/users",
    operation_id = "users_create",
    summary = "Create a user, or the root user on a fresh install",
    description = "With no user yet (`GET /v1/version` says `missing_root`), needs no \
        `Authorization`: creates the root user, holding every scope, logged in at once (its \
        session in the answer); of two racing, one is root and the other answers 409. Else \
        needs `users.$write`, and the new user's scopes must be covered by the caller's.",
    request_body = CreateRequest,
    responses(
        (status = 201, description = "Created", body = CreateResponse),
        (status = 400, description = "Malformed body, email, name, password or scopes",
            body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope, or scopes wider than the caller's",
            body = Error),
        (status = 409, description = "The email is taken, or the root user exists",
            body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn create(
    caller: Option<Caller>,
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateResponse>), Error> {
    let Some(caller) = caller else {
        if users::root_exists(&state.db, &state.root_exists).await? {
            return Err(bearer_required());
        }
        let Json(request) = request?;
        let new = request.user()?;
        if new.password.is_none() {
            return Err(Error::invalid_argument("the root user needs a password"));
        }
        let (user, issued) = users::create_root(&state.db, &state.root_exists, new).await?;
        let created = CreateResponse {
            user,
            token: Some(issued.token),
            expires_at: Some(issued.session.expires_at),
        };
        return Ok((StatusCode::CREATED, Json(created)));
    };
    scope::require!(caller, "com.hashinbox.users.$write");
    let Json(request) = request?;
    let (user, within) = (&caller.user, &caller.session.scopes);
    let user = users::create(&state.db, user, within, request.user()?).await?;
    let created = CreateResponse {
        user,
        token: None,
        expires_at: None,
    };
    Ok((StatusCode::CREATED, Json(created)))
}

/// The user a route changes.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
struct UserPath {
    /// Its id.
    id: String,
}

/// `PATCH /v1/users/{id}` body: what changes.
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = UpdateUserRequest)]
#[serde(deny_unknown_fields)]
struct UpdateRequest {
    /// Its new scopes, packed, covered by the caller's.
    #[serde(default)]
    #[schema(nullable = false, value_type = Option<std::collections::BTreeMap<String, Vec<String>>>)]
    scopes: Option<Packed>,
    /// Disabled (it, its keys and every session refused), or enabled again.
    #[serde(default)]
    #[schema(nullable = false)]
    disabled: Option<bool>,
}

#[scope::utoipa(
    patch,
    path = "/v1/users/{id}",
    operation_id = "user_update",
    summary = "Change a user's scopes, or disable it",
    description = "Narrowing a user narrows its keys and sessions at once: one wider than its \
        user answers 401 from the next request on.",
    params(UserPath),
    request_body = UpdateRequest,
    responses(
        (status = 200, description = "Changed", body = User),
        (status = 400, description = "Malformed body or scopes", body = Error),
        (status = 401, description = "Missing or invalid bearer token", body = Error),
        (status = 403, description = "Missing scope, or scopes wider than the caller's",
            body = Error),
        (status = 404, description = "No such user", body = Error),
        (status = 503, description = "The store is unavailable", body = Error)
    ),
    tag = "auth"
)]
async fn update(
    caller: Caller,
    State(state): State<Arc<AppState>>,
    Path(path): Path<UserPath>,
    request: Result<Json<UpdateRequest>, JsonRejection>,
) -> Result<Json<User>, Error> {
    scope::require!(caller, "com.hashinbox.users.$write");
    let Json(request) = request?;
    let change = UserChange {
        scopes: request.scopes,
        disabled: request.disabled,
    };
    let within = &caller.session.scopes;
    Ok(Json(
        users::update(&state.db, &path.id, within, change).await?,
    ))
}
