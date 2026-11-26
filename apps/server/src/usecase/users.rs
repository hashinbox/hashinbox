//! Users (design 096): the root user on a fresh install, users created by
//! a caller (their scopes covered by its), changed and disabled; a user's
//! password and two-factor; and whether the root exists, cached once
//! true.

use std::sync::atomic::{AtomicBool, Ordering};

use crypto::{password, random, totp};
use scope::Scopes;

use super::{
    now,
    sessions::{self, Issued, Packed, narrowed},
};
use crate::{
    auth,
    db::{Db, Page},
    error::{Error, reason},
    events,
    models::{Session, User, User2fa, session::Amr, user, user_2fa},
};

/// The issuer an authenticator app shows for a user's two-factor.
const TOTP_ISSUER: &str = "hashinbox";

/// A user asked for: `POST /v1/users`.
pub(crate) struct NewUser {
    pub(crate) email: String,
    pub(crate) name: String,
    /// `None`: no dashboard login.
    pub(crate) password: Option<String>,
    /// Packed; none when `None` (the root: every scope).
    pub(crate) scopes: Option<Packed>,
}

impl NewUser {
    /// The user asked for, checked: an email with `@`, a name, a password
    /// of 8 characters at least; the email and name trimmed.
    ///
    /// # Errors
    ///
    /// [`Error::invalid_argument`], naming the field.
    pub(crate) fn new(
        email: &str,
        name: &str,
        password: Option<String>,
        scopes: Option<Packed>,
    ) -> Result<Self, Error> {
        let (email, name) = (email.trim(), name.trim());
        if !(3..=254).contains(&email.len()) || !email.contains('@') {
            return Err(Error::invalid_argument("email is not an email"));
        }
        if !(1..=128).contains(&name.chars().count()) {
            return Err(Error::invalid_argument("name must be 1 to 128 characters"));
        }
        if let Some(password) = &password {
            user::check_password(password)?;
        }
        Ok(Self {
            email: email.to_owned(),
            name: name.to_owned(),
            password,
            scopes,
        })
    }
}

/// A change asked for: `PATCH /v1/users/{id}`.
pub(crate) struct UserChange {
    /// Its new scopes, covered by the caller's.
    pub(crate) scopes: Option<Packed>,
    /// Disabled (`true`) or enabled.
    pub(crate) disabled: Option<bool>,
}

/// `password`'s argon2id hash, on a blocking thread.
async fn hashed(password: String) -> Result<String, Error> {
    let hashed = tokio::task::spawn_blocking(move || password::hash(&password)).await;
    let hashed = hashed.map_err(|_| Error::unavailable("the password hash did not finish"))?;
    Ok(hashed?)
}

/// The user `new` asks for, holding `scopes`.
async fn user_of(new: NewUser, scopes: Scopes) -> Result<User, Error> {
    let hash = match new.password {
        Some(password) => Some(hashed(password).await?),
        None => None,
    };
    Ok(User::new(new.email, new.name, hash, scopes, now())?)
}

/// Keeps `user` on `connection`; already exists when `refuses` says so
/// first, or when the email is taken.
fn insert_on(
    connection: &mut diesel::SqliteConnection,
    user: &User,
    refuses: fn(&mut diesel::SqliteConnection) -> diesel::QueryResult<bool>,
) -> anyhow::Result<Result<(), Error>> {
    if refuses(connection)? {
        return Ok(Err(Error::already_exists("the root user exists already")));
    }
    match user::db::insert(connection, user) {
        Err(diesel::result::Error::DatabaseError(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            _,
        )) => return Ok(Err(Error::already_exists("the email is taken"))),
        kept => kept?,
    }
    Ok(Ok(()))
}

/// Keeps `user`, in one write; `refused` when `refuses` says so first
/// (on the same connection).
async fn insert(
    db: &Db,
    user: User,
    refuses: fn(&mut diesel::SqliteConnection) -> diesel::QueryResult<bool>,
) -> Result<User, Error> {
    db.write(move |connection| Ok(insert_on(connection, &user, refuses)?.map(|()| user)))
        .await?
}

/// The root user `new` asks for: every scope, its password hashed; kept
/// by [`create_root`], or by [`insert_root`] in a caller's own write.
///
/// # Errors
///
/// The hash; no randomness.
pub(crate) async fn root_of(new: NewUser) -> Result<User, Error> {
    user_of(new, auth::everything()).await
}

/// Keeps `root` on `connection` unless a user exists: [`create_root`]'s
/// write, inside a caller's own.
///
/// # Errors
///
/// Already exists when a user exists; the store.
pub(crate) fn insert_root(
    connection: &mut diesel::SqliteConnection,
    root: &User,
) -> anyhow::Result<Result<(), Error>> {
    insert_on(connection, root, user::db::any)
}

/// Whether a user exists: `root_exists` once true, else read (one
/// `EXISTS`) and kept in it when true.
///
/// # Errors
///
/// The store.
pub(crate) async fn root_exists(db: &Db, root_exists: &AtomicBool) -> Result<bool, Error> {
    if root_exists.load(Ordering::Relaxed) {
        return Ok(true);
    }
    let exists = db.read(|connection| Ok(user::db::any(connection)?)).await?;
    if exists {
        root_exists.store(true, Ordering::Relaxed);
    }
    Ok(exists)
}

/// The root user on a fresh install: every scope, logged in at once. The
/// check that no user exists and the insert are one write, so of two
/// racing only one is root; `root_exists` is set once it commits.
///
/// # Errors
///
/// Already exists when a user exists (the email is never taken: no other); the
/// store.
pub(crate) async fn create_root(
    db: &Db,
    root_exists: &AtomicBool,
    new: NewUser,
) -> Result<(User, Issued), Error> {
    let user = insert(db, root_of(new).await?, user::db::any).await?;
    root_exists.store(true, Ordering::Relaxed);
    tracing::info!(target: events::AUTH_USER_CREATED, user = user.id, email = user.email, by = "bootstrap");
    let (scopes, ttl, at) = (
        user.scopes.clone(),
        sessions::DEFAULT_TTL,
        now().div_euclid(1000),
    );
    let session = sessions::issue_for_user(&user, scopes, ttl, vec![Amr::Pwd], at);
    Ok((user, session))
}

/// A user `creator` creates, its scopes covered by `within` (the
/// creator's session's).
///
/// # Errors
///
/// Invalid or denied scopes; already exists when the email is taken; the
/// store.
pub(crate) async fn create(
    db: &Db,
    creator: &User,
    within: &Scopes,
    new: NewUser,
) -> Result<User, Error> {
    let scopes = match &new.scopes {
        Some(packed) => narrowed(packed, within)?,
        None => Scopes::new(),
    };
    let user = user_of(new, scopes).await?;
    let user = insert(db, user, |_| Ok(false)).await?;
    tracing::info!(target: events::AUTH_USER_CREATED, user = user.id, email = user.email, by = creator.id);
    Ok(user)
}

/// A page of the users, by id.
///
/// # Errors
///
/// The store.
pub(crate) async fn list_page(db: &Db, page: Page<String>) -> Result<Vec<User>, Error> {
    Ok(db
        .read(move |connection| Ok(user::db::find_page(connection, page)?))
        .await?)
}

/// Changes user `id` as `change` asks, its scopes covered by `within`
/// (the caller's session's).
///
/// # Errors
///
/// Invalid or denied scopes; not found; the store.
pub(crate) async fn update(
    db: &Db,
    id: &str,
    within: &Scopes,
    change: UserChange,
) -> Result<User, Error> {
    let scopes = change
        .scopes
        .as_ref()
        .map(|packed| narrowed(packed, within))
        .transpose()?;
    let disabled_at = change.disabled.map(|disabled| disabled.then(now));
    let id = id.to_owned();
    let changed = db
        .write(move |connection| {
            if let Some(scopes) = &scopes {
                user::db::set_scopes(connection, &id, scopes)?;
            }
            if let Some(at) = disabled_at {
                user::db::set_disabled_at(connection, &id, at)?;
            }
            Ok(user::db::find(connection, &id)?)
        })
        .await?;
    changed.ok_or_else(|| Error::not_found("no such user"))
}

/// Sets `user`'s password to `new` (checked as at signup; the current one
/// is not asked), ending every other session: `invalidated_at` is now. The
/// answer is a fresh session with `session`'s scopes and `amr`, issued the
/// next second, so the invalidation (second precision) spares it alone.
///
/// # Errors
///
/// Invalid argument for a password too short; the store.
pub(crate) async fn set_password(
    db: &Db,
    user: &User,
    session: &Session,
    new: String,
) -> Result<Issued, Error> {
    user::check_password(&new)?;
    let hash = hashed(new).await?;
    let (id, at) = (user.id.clone(), now());
    db.write(move |connection| Ok(user::db::set_password_hash(connection, &id, &hash, at)?))
        .await?;
    let (scopes, amr) = (session.scopes.clone(), session.amr.clone());
    let next_second = at.div_euclid(1000) + 1;
    let ttl = sessions::DEFAULT_TTL;
    Ok(sessions::issue_for_user(
        user,
        scopes,
        ttl,
        amr,
        next_second,
    ))
}

/// User `id`'s two-factor, if started.
///
/// # Errors
///
/// The store.
pub(crate) async fn two_factor_of(db: &Db, id: &str) -> Result<Option<User2fa>, Error> {
    let id = id.to_owned();
    Ok(db
        .read(move |connection| Ok(user_2fa::db::find(connection, &id)?))
        .await?)
}

/// Starts `user`'s two-factor: a new secret, not on until its first code
/// is confirmed; its `otpauth://` URI.
///
/// # Errors
///
/// Failed precondition when it is on already; the store.
pub(crate) async fn start_two_factor(db: &Db, user: &User) -> Result<String, Error> {
    let row = User2fa {
        user_id: user.id.clone(),
        secret: random::<20>()?.to_vec(),
        confirmed_at: None,
        last_step: None,
        created_at: now(),
    };
    let uri = totp::uri(&row.secret, TOTP_ISSUER, &user.email);
    db.write(move |connection| {
        let kept = user_2fa::db::find(connection, &row.user_id)?;
        if kept.is_some_and(|kept| kept.confirmed_at.is_some()) {
            return Ok(Err(Error::failed_precondition(
                "two-factor is on: turn it off first",
            )));
        }
        user_2fa::db::replace(connection, &row)?;
        Ok(Ok(()))
    })
    .await??;
    Ok(uri)
}

/// Uses `code` of `user`'s two-factor, turning it on if it was not.
///
/// # Errors
///
/// Unauthenticated, `wrong_credentials`, when the code is wrong or replayed; not found when no
/// two-factor was started; the store.
async fn use_code(db: &Db, user: &User, code: String) -> Result<User2fa, Error> {
    let (id, at) = (user.id.clone(), now());
    db.write(move |connection| {
        let Some(row) = user_2fa::db::find(connection, &id)? else {
            return Ok(Err(Error::not_found("two-factor was not started")));
        };
        let Some(step) = row.step_of(&code, at) else {
            return Ok(Err(Error::unauthenticated(
                "the code is wrong, or used already",
            )
            .reason(reason::WRONG_CREDENTIALS)));
        };
        user_2fa::db::use_step(connection, &id, step, at)?;
        Ok(Ok(row))
    })
    .await?
}

/// Confirms `user`'s two-factor with its first `code`: on from now.
///
/// # Errors
///
/// As [`use_code`].
pub(crate) async fn confirm_two_factor(db: &Db, user: &User, code: String) -> Result<(), Error> {
    let row = use_code(db, user, code).await?;
    if row.confirmed_at.is_none() {
        tracing::info!(target: events::AUTH_2FA_ON, user = user.id);
    }
    Ok(())
}

/// Turns `user`'s two-factor off with a valid `code`, logging it out
/// everywhere.
///
/// # Errors
///
/// As [`use_code`]; the store.
pub(crate) async fn stop_two_factor(db: &Db, user: &User, code: String) -> Result<(), Error> {
    use_code(db, user, code).await?;
    let (id, at) = (user.id.clone(), now());
    db.write(move |connection| {
        user_2fa::db::delete(connection, &id)?;
        Ok(user::db::invalidate(connection, &id, at)?)
    })
    .await?;
    tracing::info!(target: events::AUTH_2FA_OFF, user = user.id);
    Ok(())
}
