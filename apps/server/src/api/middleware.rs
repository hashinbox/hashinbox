//! Who is calling (design 096): one middleware on the whole router, and the
//! extractors reading what it found. It takes the bearer token
//! (`Authorization: Bearer …`, or `?token=` on a GET), checks it once
//! (`usecase::sessions::authenticate`) and puts the [`Caller`] in the
//! request. With no token it does nothing, and a handler's `Session` or
//! `Caller` answers 401; a bad one answers 401 at once, on any route.
//! Which scope a route needs is the route's (`scope::require!`, which
//! `#[scope::utoipa]` reads).

use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, OptionalFromRequestParts, Request, State},
    http::{Method, header::AUTHORIZATION, request::Parts},
    middleware::Next,
    response::Response,
};

use crate::{
    api::AppState,
    error::Error,
    models::{Caller, Session},
    usecase::{self, sessions::bearer_required},
};

/// The middleware: the request's token checked, its [`Caller`] put in the
/// request.
///
/// # Errors
///
/// 401 when a token is sent and refused; the store.
pub(crate) async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, Error> {
    if let Some(token) = token_of(&request)? {
        let caller = usecase::sessions::authenticate(&state.db, &token, usecase::now()).await?;
        request.extensions_mut().insert(caller);
    }
    Ok(next.run(request).await)
}

/// The token `request` sends: its `Authorization: Bearer …`, or, on a GET
/// without one, its `?token=` (an `EventSource` sets no header).
///
/// # Errors
///
/// 401 when `Authorization` is not `Bearer …`.
fn token_of(request: &Request) -> Result<Option<String>, Error> {
    if let Some(header) = request.headers().get(AUTHORIZATION) {
        let header = header.to_str().map_err(|_| bearer_required())?;
        let (scheme, token) = header.split_once(' ').ok_or_else(bearer_required)?;
        if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
            return Err(bearer_required());
        }
        return Ok(Some(token.trim().to_owned()));
    }
    if request.method() != Method::GET {
        return Ok(None);
    }
    let query = request.uri().query().unwrap_or_default();
    let mut pairs = url::form_urlencoded::parse(query.as_bytes());
    Ok(pairs
        .find(|(name, _)| name == "token")
        .map(|(_, token)| token.into_owned()))
}

/// The checked caller; 401 when none.
impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Error> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            .ok_or_else(bearer_required)
    }
}

/// The checked caller, if a token was sent.
impl<S: Send + Sync> OptionalFromRequestParts<S> for Caller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Option<Self>, Error> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

/// The checked caller's session; 401 when none.
impl<S: Send + Sync> FromRequestParts<S> for Session {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Error> {
        let caller = parts.extensions.get::<Caller>();
        let caller = caller.ok_or_else(bearer_required)?;
        Ok(caller.session.clone())
    }
}

/// The checked caller's session, if a token was sent.
impl<S: Send + Sync> OptionalFromRequestParts<S> for Session {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Option<Self>, Error> {
        let caller = parts.extensions.get::<Caller>();
        Ok(caller.map(|caller| caller.session.clone()))
    }
}
