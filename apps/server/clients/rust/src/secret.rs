//! Hand-written: a key's [`Secret`] and the sessions it signs (design 096).
//!
//! A key session is an HS256 JWT: header `{typ, alg, kid}`, claims `iss`
//! (= `kid`, the key's `hip/…` id), `sub?`, `iat`, `exp`, `scopes`
//! (packed), the HMAC key the whole `his/…` text as given. The secret
//! itself never goes over the wire.
//!
//! **Clocks:** the server takes `iat` at most 60 s ahead of its clock and a
//! session of at most 24 h, and refuses one whose `exp` is past. A machine
//! whose clock runs more than 60 s fast, or slow by more than the TTL,
//! signs sessions the server refuses: keep it synced (NTP).

use std::fmt;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::scope::{ParseError, Scope, Scopes};
use crate::session::{Claims, Header, Invalid, Session, now};

/// The longest session the server accepts.
pub const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A key: its public id `hip/…` and its secret `his/…`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    id: String,
    value: String,
}

impl fmt::Debug for Secret {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "Secret({}, his/…)", self.id)
    }
}

impl Secret {
    /// Key `id` (`hip/…`) with its secret `value` (`his/…`): base 58, the
    /// secret 32 bytes and their CRC32, checked here (a typo).
    ///
    /// # Errors
    ///
    /// When either is malformed or the CRC does not match.
    pub fn parse(id: impl Into<String>, value: impl Into<String>) -> Result<Self, Invalid> {
        let (id, value) = (id.into(), value.into());
        let id_ok = id
            .strip_prefix("hip/")
            .is_some_and(|digits| bs58::decode(digits).into_vec().is_ok_and(|b| !b.is_empty()));
        if !id_ok {
            return Err(Invalid("a key id is `hip/` and base 58".into()));
        }
        let bytes = value
            .strip_prefix("his/")
            .and_then(|digits| bs58::decode(digits).into_vec().ok())
            .filter(|bytes| bytes.len() == 36)
            .ok_or_else(|| Invalid("a secret is `his/` and 36 bytes of base 58".into()))?;
        let (random, check) = bytes.split_at(32);
        let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(random);
        if crc.to_be_bytes() != check {
            return Err(Invalid(
                "the secret's checksum does not match: a typo".into(),
            ));
        }
        Ok(Self { id, value })
    }

    /// Its key's id, `hip/…`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// A session of this key, signed here.
    ///
    /// # Errors
    ///
    /// When its TTL is zero or over [`MAX_TTL`].
    pub fn issue(&self, issue: Issue) -> Result<Session, Invalid> {
        let ttl = issue.ttl.as_secs();
        if ttl == 0 || issue.ttl > MAX_TTL {
            return Err(Invalid("a session lasts 1 s to 24 h".into()));
        }
        let iat = issue.issued_at.unwrap_or_else(now);
        let header = Header {
            typ: "JWT".into(),
            alg: "HS256".into(),
            kid: self.id.clone(),
        };
        let claims = Claims {
            iss: self.id.clone(),
            sub: issue.subject,
            iat,
            exp: iat + i64::try_from(ttl).unwrap_or(0),
            jti: None,
            scopes: issue.scopes.pack(),
            amr: Vec::new(),
        };
        let encode = |json: Vec<u8>| URL_SAFE_NO_PAD.encode(json);
        let header = encode(serde_json::to_vec(&header).expect("a header is JSON"));
        let claims = encode(serde_json::to_vec(&claims).expect("claims are JSON"));
        let input = format!("{header}.{claims}");
        let mut mac = Hmac::<Sha256>::new_from_slice(self.value.as_bytes()).expect("any key");
        mac.update(input.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Session::parse(format!("{input}.{signature}"))
    }
}

/// What a [`Secret`] signs: scopes, a subject, a TTL (10 minutes unless
/// set).
#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub(crate) scopes: Scopes,
    subject: Option<String>,
    pub(crate) ttl: Duration,
    issued_at: Option<i64>,
}

impl Default for Issue {
    fn default() -> Self {
        Self {
            scopes: Scopes::new(),
            subject: None,
            ttl: Duration::from_secs(10 * 60),
            issued_at: None,
        }
    }
}

impl Issue {
    /// No scopes, no subject, 10 minutes.
    pub fn new() -> Self {
        Self::default()
    }

    /// With the scope `text` too (`com.hashinbox.wallets[x].$read`).
    ///
    /// # Errors
    ///
    /// When `text` is not a scope.
    pub fn scope(mut self, text: &str) -> Result<Self, ParseError> {
        self.scopes.push(text.parse::<Scope>()?);
        Ok(self)
    }

    /// With `scopes` too.
    #[must_use]
    pub fn scopes(mut self, scopes: Scopes) -> Self {
        scopes.into_iter().for_each(|scope| self.scopes.push(scope));
        self
    }

    /// For `subject` (`sub`): whom it is for; logged, grants nothing.
    #[must_use]
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Lasting `ttl`, whole seconds, at most [`MAX_TTL`].
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Issued at `iat` (Unix seconds) in place of now: for test vectors.
    #[must_use]
    pub fn issued_at(mut self, iat: i64) -> Self {
        self.issued_at = Some(iat);
        self
    }
}
