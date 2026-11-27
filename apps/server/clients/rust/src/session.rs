//! Hand-written: [`Session`], a JWT the server issued (a login) or a key
//! signed ([`crate::Secret::issue`]), decoded as design 096 writes it.
//!
//! The client never checks a signature: only the server holds what checks
//! it. It reads the claims to know the scopes, the times and the issuer.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::client::Operation;
use crate::models::Amr;
use crate::operations::DEPENDS;
use crate::scope::{Scopes, escape_id, fill};

/// A session: its JWT and what the JWT says.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// The JWT, sent as `Authorization: Bearer …`.
    pub token: String,
    /// `iss` (and `kid`): `user/…` for a login, `hip/…` for a key's.
    pub issuer: String,
    /// `sub`: a user's id, or whom a key's session is for.
    pub subject: Option<String>,
    /// `scopes`, unpacked and expanded with [`DEPENDS`]: what the server
    /// checks.
    pub scopes: Scopes,
    /// `iat`, Unix seconds.
    pub issued_at: i64,
    /// `exp`, Unix seconds.
    pub expires_at: i64,
    /// `amr`: how a login proved who it is; empty for a key's.
    pub amr: Vec<Amr>,
}

/// Why a token, a secret or a session to issue is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid(pub(crate) String);

impl fmt::Display for Invalid {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

impl From<::scope::ParseError> for Invalid {
    fn from(error: ::scope::ParseError) -> Self {
        Self(error.to_string())
    }
}

/// A JWT's header, in `jsonwebtoken`'s field order (the server's).
#[derive(Serialize, Deserialize)]
pub(crate) struct Header {
    pub(crate) typ: String,
    pub(crate) alg: String,
    pub(crate) kid: String,
}

/// A JWT's claims, in the server's field order; the server refuses others.
#[derive(Serialize, Deserialize)]
pub(crate) struct Claims {
    pub(crate) iss: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sub: Option<String>,
    pub(crate) iat: i64,
    pub(crate) exp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) jti: Option<String>,
    pub(crate) scopes: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) amr: Vec<Amr>,
}

impl Session {
    /// The session `token` is: decoded, **not verified**. Its header must
    /// be HS256 with `kid` its `iss`, its scopes must parse.
    ///
    /// # Errors
    ///
    /// When it is not such a JWT.
    pub fn parse(token: impl Into<String>) -> Result<Self, Invalid> {
        let token = token.into();
        let mut parts = token.split('.');
        let (Some(header), Some(claims), Some(_), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Invalid("not a JWT".into()));
        };
        let header: Header = part(header)?;
        let claims: Claims = part(claims)?;
        if header.alg != "HS256" || header.kid != claims.iss {
            return Err(Invalid("not HS256, or its kid is not its iss".into()));
        }
        Ok(Self {
            scopes: Scopes::unpack(&claims.scopes)?.expand(DEPENDS),
            issuer: claims.iss,
            subject: claims.sub,
            issued_at: claims.iat,
            expires_at: claims.exp,
            amr: claims.amr,
            token,
        })
    }

    /// Whether `exp` is past, by this machine's clock.
    pub fn expired(&self) -> bool {
        self.expires_at <= now()
    }

    /// Whether it holds every scope `operation` needs, its holes filled
    /// from `params` (`[("id", "ethereum.mainnet.0xabc")]`).
    pub fn can(&self, operation: &Operation, params: &[(&str, &str)]) -> bool {
        let param = |name: &str| {
            let value = params.iter().find(|(key, _)| *key == name);
            escape_id(value.map_or("", |(_, value)| value))
        };
        let templates = operation.templates.iter();
        templates
            .map(|template| fill(template, param))
            .all(|scope| crate::client::Session::covers(self, &scope))
    }
}

impl crate::client::Session for Session {
    fn covers(&self, scope: &str) -> bool {
        scope.parse().is_ok_and(|scope| self.scopes.covers(&scope))
    }
}

/// Now, Unix seconds.
pub(crate) fn now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |since| since.as_secs().try_into().unwrap_or(i64::MAX))
}

/// A JWT part, base64url JSON, decoded.
fn part<T: DeserializeOwned>(text: &str) -> Result<T, Invalid> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| Invalid("a JWT part is not base64url".into()))?;
    serde_json::from_slice(&bytes).map_err(|error| Invalid(format!("a JWT part: {error}")))
}
