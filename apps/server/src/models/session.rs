//! [`Session`]: the data a JWT carries (design 096), and nothing else. No
//! table: every JWT is a session, HS256, issued by a user (the server signs
//! it at login with the user's `session_secret`) or by a key (signed with
//! its secret's text, `his/…`), told apart by its [`Client`]. Decoding it
//! checks its times; only its issuer holds what checks its signature
//! ([`Session::is_signed_by`]). `jsonwebtoken` encodes and decodes it.

use std::{collections::BTreeMap, fmt};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use scope::Scopes;
use serde::{Deserialize, Serialize, de::IgnoredAny};

use super::{user::UserId, user_key::UserKeyId};

/// The longest a session lasts: 24 hours, in seconds.
pub(crate) const MAX_TTL: i64 = 24 * 60 * 60;
/// How far in the future a session's `iat` may be: clock skew, seconds.
const SKEW: i64 = 60;
/// The prefix of a user's client id.
const USER_PREFIX: &str = "user/";
/// The prefix of a key's client id (part of its id).
const KEY_PREFIX: &str = "hip/";

/// Who issued a session: its public client id, the JWT's `kid` and `iss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Client {
    /// `user/{id}`: a user, signed with its `session_secret`.
    User(UserId),
    /// `hip/…`: a key, signed with its secret.
    Key(UserKeyId),
}

impl Client {
    /// The client `id` names; `None` for neither kind.
    fn parse(id: &str) -> Option<Self> {
        if let Some(user) = id.strip_prefix(USER_PREFIX) {
            return (!user.is_empty()).then(|| Self::User(user.to_owned()));
        }
        let key = id.strip_prefix(KEY_PREFIX)?;
        (!key.is_empty()).then(|| Self::Key(id.to_owned()))
    }
}

impl fmt::Display for Client {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(id) => write!(out, "{USER_PREFIX}{id}"),
            Self::Key(id) => out.write_str(id),
        }
    }
}

/// How a user session's login proved who it is: the JWT's standard `amr`
/// claim (RFC 8176), each method once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Amr {
    /// The user's password.
    Pwd,
    /// The instance's recovery passphrase (12 BIP39 words).
    Passphrase,
    /// A two-factor (TOTP) code.
    Otp,
}

impl Amr {
    /// Its claim value: `pwd`, `passphrase` or `otp`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Pwd => "pwd",
            Self::Passphrase => "passphrase",
            Self::Otp => "otp",
        }
    }
}

/// Why a JWT is not a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Invalid(&'static str);

impl Invalid {
    /// Its `exp` is past: a genuine session may have been this, so it is told
    /// apart from the other refusals.
    pub(crate) const EXPIRED: Self = Self("expired");
}

impl fmt::Display for Invalid {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.0)
    }
}

impl std::error::Error for Invalid {}

/// A JWT's claims, its fields fixed.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    /// The issuer's client id, as the header's `kid`.
    iss: String,
    /// A key's session: anything its third party wants (a customer);
    /// a user's: its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sub: Option<String>,
    /// Unix seconds.
    iat: i64,
    /// Unix seconds.
    exp: i64,
    /// Its id; a third party may leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jti: Option<String>,
    /// What it may do, packed (`Scopes::pack`).
    scopes: BTreeMap<String, Vec<String>>,
    /// How its login proved who it is; none for a key's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    amr: Vec<Amr>,
}

/// A session: who issued it, what it may do and when. A handler only sees
/// one its issuer has checked (`api::middleware`).
#[derive(Clone, Debug)]
pub(crate) struct Session {
    /// Who issued it (`kid`, `iss`).
    pub(crate) client: Client,
    /// `sub`: logged, grants nothing.
    pub(crate) sub: Option<String>,
    /// What it may do: the scopes that count.
    pub(crate) scopes: Scopes,
    /// `iat`, Unix seconds.
    pub(crate) issued_at: i64,
    /// `exp`, Unix seconds.
    pub(crate) expires_at: i64,
    /// `jti`, when it has one.
    pub(crate) id: Option<String>,
    /// `amr`: how its login proved who it is (`pwd` or `passphrase`, then
    /// `otp` when a code was used); empty for a key's.
    pub(crate) amr: Vec<Amr>,
    /// The JWT it was decoded from, for its issuer to check; `None` for a
    /// session made here (a new one, or a bearer secret's).
    jwt: Option<String>,
}

/// How `jsonwebtoken` checks a session: HS256 only (`alg: none` or any
/// other algorithm refused), and none of its own claim checks (`exp`,
/// `nbf`, `aud`, required claims, leeway 0): [`Session::from_jwt`] checks
/// the times against the `now` it is given.
fn validation() -> Validation {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.leeway = 0;
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();
    validation
}

impl Session {
    /// A new session `client` issues at `now` (Unix seconds) for `ttl`
    /// seconds, with a new id.
    pub(crate) fn new(
        client: Client,
        sub: Option<String>,
        scopes: Scopes,
        now: i64,
        ttl: i64,
    ) -> Self {
        Self {
            client,
            sub,
            scopes,
            issued_at: now,
            expires_at: now + ttl,
            id: Some(uuid::Uuid::now_v7().to_string()),
            amr: Vec::new(),
            jwt: None,
        }
    }

    /// The session `jwt` holds: decoded without checking its signature
    /// (only its issuer holds what checks it), its header HS256, typed
    /// `JWT` and its `kid` its `iss`, `iat` not in the future, `exp` after
    /// `now` (Unix seconds) and at most 24 hours after `iat`.
    ///
    /// # Errors
    ///
    /// When it is not such a JWT.
    pub(crate) fn from_jwt(jwt: &str, now: i64) -> Result<Self, Invalid> {
        let mut unverified = validation();
        unverified.insecure_disable_signature_validation();
        let decoded =
            jsonwebtoken::decode::<Claims>(jwt, &DecodingKey::from_secret(&[]), &unverified)
                .map_err(|_| Invalid("not a session JWT"))?;
        let (header, claims) = (decoded.header, decoded.claims);
        if header.alg != Algorithm::HS256 {
            return Err(Invalid("its alg is not HS256"));
        }
        if header.typ.as_deref() != Some("JWT") || header.kid.as_deref() != Some(&claims.iss) {
            return Err(Invalid("its header does not name its issuer"));
        }
        let client = Client::parse(&claims.iss).ok_or(Invalid("its issuer is not a client id"))?;
        if claims.iat > now + SKEW || claims.exp - claims.iat > MAX_TTL {
            return Err(Invalid("issued in the future, or valid too long"));
        }
        if claims.exp <= now {
            return Err(Invalid::EXPIRED);
        }
        let scopes =
            Scopes::unpack(&claims.scopes).map_err(|_| Invalid("its scopes do not parse"))?;
        Ok(Self {
            client,
            sub: claims.sub,
            scopes,
            issued_at: claims.iat,
            expires_at: claims.exp,
            id: claims.jti,
            amr: claims.amr,
            jwt: Some(jwt.to_owned()),
        })
    }

    /// Its JWT, HS256 with `key` (a user's `session_secret`, or a key's
    /// secret text `his/…` as it is), `kid` its issuer.
    pub(crate) fn to_jwt(&self, key: &[u8]) -> String {
        let claims = Claims {
            iss: self.client.to_string(),
            sub: self.sub.clone(),
            iat: self.issued_at,
            exp: self.expires_at,
            jti: self.id.clone(),
            scopes: self.scopes.pack(),
            amr: self.amr.clone(),
        };
        let header = Header {
            kid: Some(claims.iss.clone()),
            ..Header::new(Algorithm::HS256)
        };
        // HMAC takes any key, and the claims have no map with non-text
        // keys: it never fails.
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(key)).unwrap_or_default()
    }

    /// Whether `key` (its user's `session_secret`, or its key's secret
    /// text) signed the JWT it was decoded from, HS256 only; false for a
    /// session not decoded from one.
    pub(crate) fn is_signed_by(&self, key: &[u8]) -> bool {
        self.jwt.as_deref().is_some_and(|jwt| {
            jsonwebtoken::decode::<IgnoredAny>(jwt, &DecodingKey::from_secret(key), &validation())
                .is_ok()
        })
    }

    /// Whether `invalidated_at` (Unix ms, its issuer's) refuses it: a JWT
    /// issued in that second or before. A session not decoded from a JWT
    /// (a key's bearer secret) was issued by no one, and none refuses it:
    /// only revoking the key does.
    pub(crate) fn is_invalidated_by(&self, invalidated_at: Option<i64>) -> bool {
        let issued = self.issued_at;
        self.jwt.is_some() && invalidated_at.is_some_and(|at| issued <= at.div_euclid(1000))
    }

    /// Whether the recovery passphrase, set at `set_at` (Unix ms; `None`
    /// when none is), refuses it: a passphrase login's session issued in
    /// that second or before, or any once none is set. Replacing or
    /// removing the passphrase so ends every session its old words made.
    pub(crate) fn is_ended_by_recovery(&self, set_at: Option<i64>) -> bool {
        let issued = self.issued_at;
        self.amr.contains(&Amr::Passphrase) && set_at.is_none_or(|at| issued <= at.div_euclid(1000))
    }
}

impl scope::Granted for Session {
    fn scopes(&self) -> &Scopes {
        &self.scopes
    }
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{Algorithm, EncodingKey, Header};

    use super::{Amr, Client, Invalid, MAX_TTL, Session};
    use crate::models::user_key::KeySecret;

    fn scopes(text: &str) -> scope::Scopes {
        text.parse().unwrap()
    }

    #[test]
    fn a_user_session_round_trips_and_checks_its_secret() {
        let client = Client::User("0193".to_owned());
        let session = Session::new(
            client.clone(),
            None,
            scopes("com.hashinbox.wallets.$read"),
            1000,
            60,
        );
        let jwt = session.to_jwt(b"secret");
        let decoded = Session::from_jwt(&jwt, 1001).unwrap();
        assert_eq!(decoded.client, client);
        assert_eq!(decoded.scopes, session.scopes.compress());
        assert_eq!((decoded.issued_at, decoded.expires_at), (1000, 1060));
        assert_eq!(decoded.id, session.id);
        assert!(decoded.amr.is_empty());
        let mut with_amr = session.clone();
        with_amr.amr = vec![Amr::Passphrase, Amr::Otp];
        let amr = Session::from_jwt(&with_amr.to_jwt(b"secret"), 1001)
            .unwrap()
            .amr;
        assert_eq!(amr, [Amr::Passphrase, Amr::Otp]);
        assert!(decoded.is_signed_by(b"secret"));
        assert!(!decoded.is_signed_by(b"other"));
        assert!(!session.is_signed_by(b"secret"), "not decoded");
        assert_eq!(Session::from_jwt(&jwt, 1060).unwrap_err(), Invalid::EXPIRED);
    }

    #[test]
    fn a_key_session_is_hs256_with_its_secret_text_and_lasts_at_most_a_day() {
        let secret = KeySecret::generate().unwrap();
        let key = secret.reveal().as_bytes();
        let client = Client::Key("hip/abc".to_owned());
        let sub = Some("customer-1".to_owned());
        let session = Session::new(client.clone(), sub.clone(), scopes(""), 1000, MAX_TTL);
        let jwt = session.to_jwt(key);
        let decoded = Session::from_jwt(&jwt, 1000).unwrap();
        assert_eq!((&decoded.client, &decoded.sub), (&client, &sub));
        assert!(decoded.is_signed_by(key));
        let other = KeySecret::generate().unwrap();
        assert!(!decoded.is_signed_by(other.reveal().as_bytes()));
        let long = Session::new(client.clone(), None, scopes(""), 1000, MAX_TTL + 1);
        assert!(Session::from_jwt(&long.to_jwt(key), 1000).is_err());
        let future = Session::new(client, None, scopes(""), 2000, 60);
        assert!(Session::from_jwt(&future.to_jwt(key), 1000).is_err());
    }

    #[test]
    fn a_jwt_of_another_algorithm_is_refused() {
        let claims = serde_json::json!({"iss": "user/u", "iat": 1000, "exp": 1060, "scopes": {}});
        let key = EncodingKey::from_secret(b"secret");
        let sign = |alg| {
            let header = Header {
                kid: Some("user/u".to_owned()),
                ..Header::new(alg)
            };
            jsonwebtoken::encode(&header, &claims, &key).unwrap()
        };
        let hs256 = Session::from_jwt(&sign(Algorithm::HS256), 1000).unwrap();
        assert!(hs256.is_signed_by(b"secret"));
        assert!(Session::from_jwt(&sign(Algorithm::HS384), 1000).is_err());
    }

    /// The signing vectors the clients read
    /// (`apps/server/clients/_golden/session.json`): a fixed key secret and
    /// claims, and the exact JWT this server makes of them. Fails when the
    /// file is stale; `BLESS=1` rewrites it.
    #[test]
    fn signing_vectors_are_current() {
        let mut bytes: Vec<u8> = (1..=32).collect();
        bytes.extend(crypto::crc32(&bytes).to_be_bytes());
        let secret = format!("his/{}", crypto::base58::encode(&bytes));
        assert!(KeySecret::parse(&secret).is_some());
        let id = format!("hip/{}", crypto::base58::encode(&[7; 16]));
        let session = |client: Client,
                       sub: Option<&str>,
                       text: &str,
                       ttl: i64,
                       jti: Option<&str>,
                       amr: Vec<Amr>| Session {
            client,
            sub: sub.map(str::to_owned),
            scopes: scopes(text),
            issued_at: 1_700_000_000,
            expires_at: 1_700_000_000 + ttl,
            id: jti.map(str::to_owned),
            amr,
            jwt: None,
        };
        let key = |sub: Option<&str>, text: &str, ttl: i64| {
            let made = session(Client::Key(id.clone()), sub, text, ttl, None, Vec::new());
            serde_json::json!({
                "sub": sub,
                "scopes": text,
                "iat": made.issued_at,
                "exp": made.expires_at,
                "token": made.to_jwt(secret.as_bytes()),
            })
        };
        let user = session(
            Client::User("0193a".to_owned()),
            Some("0193a"),
            "com.hashinbox.*.$* 123.a.$read",
            3600,
            Some("0193b"),
            vec![Amr::Pwd, Amr::Otp],
        );
        let vectors = serde_json::json!({
            "id": id,
            "secret": secret,
            "sessions": [
                key(None, "com.hashinbox.wallets[ethereum.mainnet.0xabc].$read", 900),
                key(
                    Some("customer-1"),
                    "com.hashinbox.wallets[a,b%5D%20c%25d].$read com.hashinbox.markets.$read \
                     com.hashinbox.wallets[é].legs.$read other.thing.$write 123.a.$read",
                    MAX_TTL,
                ),
                key(Some("\"quoted\" \\ ü"), "", 60),
            ],
            "user": {
                "token": user.to_jwt(b"user secret"),
                "iss": "user/0193a",
                "sub": "0193a",
                "iat": user.issued_at,
                "exp": user.expires_at,
                "scopes": user.scopes.compress().to_string(),
                "amr": ["pwd", "otp"],
            },
        });
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/clients/_golden/session.json");
        let text = serde_json::to_string_pretty(&vectors).unwrap() + "\n";
        if std::env::var_os("BLESS").is_some() {
            std::fs::write(path, &text).unwrap();
            return;
        }
        let kept = std::fs::read_to_string(path).unwrap_or_default();
        assert!(kept == text, "{path} is stale: run with BLESS=1");
    }

    #[test]
    fn invalidation_refuses_sessions_issued_in_its_second_or_before() {
        let new = Session::new(Client::User("u".to_owned()), None, scopes(""), 1000, 60);
        assert!(!new.is_invalidated_by(Some(1_000_999)), "a bearer secret's");
        let jwt = new.to_jwt(b"secret");
        let session = Session::from_jwt(&jwt, 1000).unwrap();
        assert!(session.is_invalidated_by(Some(1_000_999)));
        assert!(session.is_invalidated_by(Some(1_001_000)));
        assert!(!session.is_invalidated_by(Some(999_999)));
        assert!(!session.is_invalidated_by(None));
    }
}
