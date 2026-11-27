//! Sessions (design 096): who a bearer token says is calling, checked
//! ([`authenticate`]); a login with a password or the recovery passphrase
//! (with its two-factor code), a key's session signed here for a caller
//! that cannot sign, logging out everywhere, and the scope narrowing every
//! issue checks.

use std::collections::BTreeMap;

use crypto::{mnemonic::Mnemonic, password};
use diesel::{QueryResult, SqliteConnection};
use scope::Scopes;

use super::now;
use crate::error::{Error, reason};
use crate::{
    auth,
    db::Db,
    events,
    models::{
        Caller, Client, Session, User, UserKey,
        session::{Amr, Invalid, MAX_TTL},
        setting, user, user_2fa,
        user_key::{self, KeySecret},
    },
};

/// A session's length when none is asked: 12 hours, seconds.
pub(crate) const DEFAULT_TTL: i64 = 12 * 60 * 60;

/// Scopes as [`Scopes::pack`] writes them: what a request body carries.
pub(crate) type Packed = BTreeMap<String, Vec<String>>;

/// A missing, malformed or forged bearer token: 401, no reason.
pub(crate) fn bearer_required() -> Error {
    Error::unauthenticated("a valid bearer token is required")
}

/// A genuine token no longer accepted (expired, invalidated, revoked, its
/// user disabled or its scopes narrowed): 401, `session_expired`.
fn session_expired() -> Error {
    Error::unauthenticated("the session expired or was ended: log in again")
        .reason(reason::SESSION_EXPIRED)
}

/// Who `token` (a key's secret `his/…`, or a session JWT) says is calling
/// at `now` (Unix ms), checked: its issuer read, then each model's rule.
///
/// # Errors
///
/// 401 when malformed or forged, or its key or user is not kept;
/// `session_expired` when genuine but no longer accepted; the store.
pub(crate) async fn authenticate(db: &Db, token: &str, now: i64) -> Result<Caller, Error> {
    if token.starts_with(user_key::SECRET_PREFIX) {
        let secret = KeySecret::parse(token).ok_or_else(bearer_required)?;
        return by_secret(db, &secret, now).await;
    }
    let session = Session::from_jwt(token, now.div_euclid(1000)).map_err(|invalid| {
        if invalid == Invalid::EXPIRED {
            session_expired()
        } else {
            bearer_required()
        }
    })?;
    match session.client.clone() {
        Client::User(id) => by_user_session(db, id, session).await,
        Client::Key(id) => by_key_session(db, id, session, now).await,
    }
}

/// A key's bearer `secret`: its key and user, and a session of the key's
/// scopes issued now, signed by no one.
async fn by_secret(db: &Db, secret: &KeySecret, now: i64) -> Result<Caller, Error> {
    let hash = secret.hash();
    let found = db
        .read(move |connection| {
            let key = user_key::db::find_by_secret_hash(connection, &hash)?;
            Ok(with_user(connection, key)?)
        })
        .await?;
    let (key, user) = found.ok_or_else(bearer_required)?;
    let client = Client::Key(key.id.clone());
    let session = Session::new(client, None, key.scopes.clone(), now.div_euclid(1000), 0);
    if !(key.accepts(&session, now) && user.accepts(&session, Some(&key), None)) {
        return Err(session_expired());
    }
    let key = Some(key);
    Ok(Caller { user, key, session })
}

/// A user's `session` (`kid: user/{id}`): signed with user `id`'s session
/// secret, and accepted by it.
async fn by_user_session(db: &Db, id: String, session: Session) -> Result<Caller, Error> {
    let passphrase = session.amr.contains(&Amr::Passphrase);
    let (user, recovery_set_at) = db
        .read(move |connection| {
            let user = user::db::find(connection, &id)?;
            Ok((user, recovery_set_at(connection, passphrase)?))
        })
        .await?;
    let user = user
        .filter(|user| session.is_signed_by(&user.session_secret))
        .ok_or_else(bearer_required)?;
    if !user.accepts(&session, None, recovery_set_at) {
        return Err(session_expired());
    }
    Ok(Caller {
        user,
        key: None,
        session,
    })
}

/// A key's `session` (`kid: hip/…`, key `id`) at `now` (Unix ms): signed
/// with the key's secret, and accepted by the key and its user.
async fn by_key_session(db: &Db, id: String, session: Session, now: i64) -> Result<Caller, Error> {
    let passphrase = session.amr.contains(&Amr::Passphrase);
    let (found, recovery_set_at) = db
        .read(move |connection| {
            let key = user_key::db::find(connection, &id)?;
            let found = with_user(connection, key)?;
            Ok((found, recovery_set_at(connection, passphrase)?))
        })
        .await?;
    let (key, user) = found
        .filter(|(key, _)| session.is_signed_by(key.secret.as_bytes()))
        .ok_or_else(bearer_required)?;
    if !(key.accepts(&session, now) && user.accepts(&session, Some(&key), recovery_set_at)) {
        return Err(session_expired());
    }
    let key = Some(key);
    Ok(Caller { user, key, session })
}

/// `key`, if kept, with its user, if kept.
fn with_user(
    connection: &mut SqliteConnection,
    key: Option<UserKey>,
) -> QueryResult<Option<(UserKey, User)>> {
    let Some(key) = key else {
        return Ok(None);
    };
    Ok(user::db::find(connection, &key.user_id)?.map(|user| (key, user)))
}

/// When the recovery passphrase was set (Unix ms), read only for a
/// `passphrase` login's session, the one it can end.
fn recovery_set_at(
    connection: &mut SqliteConnection,
    passphrase: bool,
) -> QueryResult<Option<i64>> {
    if !passphrase {
        return Ok(None);
    }
    setting::db::updated_at(connection, setting::RECOVERY_PASSPHRASE)
}

/// What a login proves who it is with.
pub(crate) enum Credential {
    /// The user's password.
    Password(String),
    /// The instance's recovery passphrase: 12 BIP39 words, for any user.
    Passphrase(String),
}

impl Credential {
    /// Its `amr` method, as logged.
    fn amr(&self) -> Amr {
        match self {
            Self::Password(_) => Amr::Pwd,
            Self::Passphrase(_) => Amr::Passphrase,
        }
    }
}

/// A login asked for: `POST /v1/session` with a password or the recovery
/// passphrase.
pub(crate) struct Login {
    pub(crate) email: String,
    pub(crate) credential: Credential,
    /// The two-factor code, when the user has it on.
    pub(crate) totp: Option<String>,
    /// Fewer scopes than the user's.
    pub(crate) scopes: Option<Packed>,
    /// Seconds, at most a day.
    pub(crate) ttl: Option<i64>,
}

/// A session issued and its JWT.
pub(crate) struct Issued {
    /// The JWT: the bearer token.
    pub(crate) token: String,
    pub(crate) session: Session,
}

/// `packed` as scopes the schema knows, covered by `within` (the
/// issuer's), each expanded: what a session, key or user may be issued.
///
/// # Errors
///
/// Invalid argument when one does not parse or is unknown; permission denied when one is
/// wider than `within`.
pub(crate) fn narrowed(packed: &Packed, within: &Scopes) -> Result<Scopes, Error> {
    let scopes = Scopes::unpack(packed)
        .map_err(|error| Error::invalid_argument(format!("a scope does not parse: {error}")))?;
    if let Some(unknown) = auth::first_unknown(&scopes) {
        return Err(Error::invalid_argument(format!(
            "{unknown} is not a known scope"
        )));
    }
    if let Some(wider) = auth::expanded(within).first_uncovered(&auth::expanded(&scopes)) {
        return Err(Error::permission_denied(format!(
            "{wider} is wider than the issuer's scopes"
        )));
    }
    Ok(scopes.compress())
}

/// `ttl`, or the default, checked: 1 second to a day.
fn ttl_of(ttl: Option<i64>) -> Result<i64, Error> {
    let ttl = ttl.unwrap_or(DEFAULT_TTL);
    if !(1..=MAX_TTL).contains(&ttl) {
        return Err(Error::invalid_argument(format!(
            "ttl must be 1 to {MAX_TTL} seconds"
        )));
    }
    Ok(ttl)
}

/// A session of `user`, signed with its session secret, of `scopes`,
/// issued at `issued_at` (Unix seconds) for `ttl` seconds, its login's
/// methods `amr`.
pub(crate) fn issue_for_user(
    user: &User,
    scopes: Scopes,
    ttl: i64,
    amr: Vec<Amr>,
    issued_at: i64,
) -> Issued {
    let client = Client::User(user.id.clone());
    let mut session = Session::new(client, Some(user.id.clone()), scopes, issued_at, ttl);
    session.amr = amr;
    Issued {
        token: session.to_jwt(&user.session_secret),
        session,
    }
}

/// The session `login` asks for, with its user. A wrong email, password,
/// passphrase or code answers the same, after the same hash check: an
/// unknown email's password is checked against a decoy hash, and the
/// passphrase against the recovery hash (the decoy when none is set)
/// whatever the email.
///
/// It is issued as of the read that found the hash it was checked against:
/// a password or passphrase replaced since ends it (`invalidated_at`,
/// `Session::is_ended_by_recovery`), however long the check took.
///
/// # Errors
///
/// Unauthenticated, `wrong_credentials`, for a wrong email, password,
/// passphrase or code (or a disabled user), `totp_required` when the rest is
/// right and no code was sent; invalid or denied scopes or ttl; the store.
pub(crate) async fn login(db: &Db, login: Login) -> Result<(Issued, User), Error> {
    let email = login.email.trim().to_owned();
    let method = login.credential.amr();
    let (by, passphrase) = (email.clone(), method == Amr::Passphrase);
    let asked_at = now().div_euclid(1000);
    let (user, recovery) = db
        .read(move |connection| {
            let user = user::db::find_by_email(connection, &by)?;
            let recovery = if passphrase {
                setting::recovery_passphrase(connection)?
            } else {
                None
            };
            Ok((user, recovery))
        })
        .await?;
    let (typed, hash) = match login.credential {
        Credential::Password(typed) => (
            typed,
            user.as_ref().and_then(|user| user.password_hash.clone()),
        ),
        Credential::Passphrase(typed) => {
            let typed = Mnemonic::parse(&typed).map_or(typed, |words| words.to_string());
            (typed, recovery)
        }
    };
    let hash = hash.unwrap_or_else(|| password::DECOY.clone());
    let right = tokio::task::spawn_blocking(move || password::verify(&typed, &hash))
        .await
        .map_err(|_| Error::unavailable("the password check did not finish"))?;
    let user = user.filter(|user| right && user.disabled_at.is_none());
    let Some(user) = user else {
        tracing::info!(target: events::AUTH_LOGIN_FAILED, email, method = method.name());
        return Err(wrong_credentials());
    };
    let scopes = match &login.scopes {
        Some(packed) => narrowed(packed, &user.scopes)?,
        None => user.scopes.clone(),
    };
    let ttl = ttl_of(login.ttl)?;
    let (id, totp, at) = (user.id.clone(), login.totp, now());
    let second = db
        .write(move |connection| {
            let row = user_2fa::db::find(connection, &id)?;
            let Some(row) = row.filter(|row| row.confirmed_at.is_some()) else {
                return Ok(Ok(false));
            };
            let Some(code) = totp else {
                return Ok(Err(Error::unauthenticated("a two-factor code is required")
                    .reason(reason::TOTP_REQUIRED)));
            };
            let Some(step) = row.step_of(&code, at) else {
                return Ok(Err(wrong_credentials()));
            };
            user_2fa::db::use_step(connection, &id, step, at)?;
            Ok(Ok(true))
        })
        .await?;
    let coded = match second {
        Ok(coded) => coded,
        Err(error) => {
            if error.reason == Some(reason::WRONG_CREDENTIALS) {
                tracing::info!(target: events::AUTH_LOGIN_FAILED, email, method = method.name());
            }
            return Err(error);
        }
    };
    tracing::info!(target: events::AUTH_LOGIN_OK, email, user = user.id, method = method.name());
    let amr = [Some(method), coded.then_some(Amr::Otp)]
        .into_iter()
        .flatten();
    let issued = issue_for_user(&user, scopes, ttl, amr.collect(), asked_at);
    Ok((issued, user))
}

/// A login's refusal, never saying which part is wrong.
fn wrong_credentials() -> Error {
    Error::unauthenticated("email, password, passphrase or code is wrong")
        .reason(reason::WRONG_CREDENTIALS)
}

/// A session of `key`, signed here with its `secret` (the caller's
/// bearer), for `sub`, of `scopes` (the key's when `None`) for `ttl`
/// seconds.
///
/// # Errors
///
/// Invalid or denied scopes or ttl.
pub(crate) fn issue_for_key(
    key: &UserKey,
    secret: &KeySecret,
    sub: Option<String>,
    scopes: Option<&Packed>,
    ttl: Option<i64>,
) -> Result<Issued, Error> {
    let scopes = match scopes {
        Some(packed) => narrowed(packed, &key.scopes)?,
        None => key.scopes.clone(),
    };
    let client = Client::Key(key.id.clone());
    let session = Session::new(client, sub, scopes, now().div_euclid(1000), ttl_of(ttl)?);
    Ok(Issued {
        token: session.to_jwt(secret.reveal().as_bytes()),
        session,
    })
}

/// Logs out everywhere: every session `session`'s issuer (its user, or
/// its key) issued until now is refused.
///
/// # Errors
///
/// The store.
pub(crate) async fn invalidate_issuer(
    db: &Db,
    user: &User,
    session: &Session,
) -> Result<(), Error> {
    let (user_id, client, at) = (user.id.clone(), session.client.clone(), now());
    db.write(move |connection| {
        match &client {
            Client::User(_) => {
                user::db::invalidate(connection, &user_id, at)?;
            }
            Client::Key(id) => {
                user_key::db::invalidate(connection, &user_id, id, at)?;
            }
        }
        Ok(())
    })
    .await?;
    Ok(())
}
