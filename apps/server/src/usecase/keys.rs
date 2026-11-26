//! Keys (design 096): created by their user (a session, or a key holding
//! `keys.$write`), their scopes covered by the creator's, the secret shown
//! once; listed without secrets; revoked; rotated (every session it signed
//! until now refused).

use scope::Scopes;

use super::{
    now,
    sessions::{Packed, narrowed},
};
use crate::{
    db::{Db, Page},
    error::Error,
    events,
    models::{
        User, UserKey,
        user_key::{self, KeySecret},
    },
};

/// A key asked for: `POST /v1/user/keys`.
pub(crate) struct NewKey {
    /// What it is for.
    pub(crate) name: String,
    /// Packed; the creator's when `None`.
    pub(crate) scopes: Option<Packed>,
    /// Unix ms it stops working; `None`: never.
    pub(crate) expires_at: Option<i64>,
}

/// A key of `user` it asked for, its scopes covered by `within` (the
/// creator's session's); and its secret's text, shown this once.
///
/// # Errors
///
/// Invalid or denied scopes; the OS's random bytes; the store.
pub(crate) async fn create(
    db: &Db,
    user: &User,
    within: &Scopes,
    new: NewKey,
) -> Result<(UserKey, String), Error> {
    let scopes = match &new.scopes {
        Some(packed) => narrowed(packed, within)?,
        None => within.clone(),
    };
    let secret = KeySecret::generate()?;
    let key = UserKey::new(
        user.id.clone(),
        new.name,
        scopes,
        new.expires_at,
        &secret,
        now(),
    )?;
    let kept = key.clone();
    db.write(move |connection| Ok(user_key::db::insert(connection, &kept)?))
        .await?;
    tracing::info!(target: events::AUTH_KEY_CREATED, key = key.id, user = user.id);
    Ok((key, secret.reveal().to_owned()))
}

/// A page of `user_id`'s keys, by id; never a secret.
///
/// # Errors
///
/// The store.
pub(crate) async fn list_page(
    db: &Db,
    user_id: &str,
    page: Page<String>,
) -> Result<Vec<UserKey>, Error> {
    let user_id = user_id.to_owned();
    Ok(db
        .read(move |connection| Ok(user_key::db::find_page_of_user(connection, &user_id, page)?))
        .await?)
}

/// Runs `change` (revoke or invalidate) on `user`'s key `id` at now: the
/// key, changed.
async fn change(
    db: &Db,
    user: &User,
    id: &str,
    change: fn(&mut diesel::SqliteConnection, &str, &str, i64) -> diesel::QueryResult<usize>,
) -> Result<UserKey, Error> {
    let (user_id, id, at) = (user.id.clone(), id.to_owned(), now());
    let changed = db
        .write(move |connection| {
            if change(connection, &user_id, &id, at)? == 0 {
                return Ok(None);
            }
            Ok(user_key::db::find(connection, &id)?)
        })
        .await?;
    changed.ok_or_else(|| Error::not_found("no such key, or revoked already"))
}

/// Revokes `user`'s key `id`: it and every session it signed refused for
/// good.
///
/// # Errors
///
/// Not found when `user` has no such key, or it is revoked; the store.
pub(crate) async fn revoke(db: &Db, user: &User, id: &str) -> Result<(), Error> {
    change(db, user, id, user_key::db::revoke).await?;
    tracing::info!(target: events::AUTH_KEY_REVOKED, key = id, user = user.id);
    Ok(())
}

/// Rotates `user`'s key `id`: every session it signed until now refused;
/// the key keeps working.
///
/// # Errors
///
/// Not found when `user` has no such key; the store.
pub(crate) async fn rotate(db: &Db, user: &User, id: &str) -> Result<UserKey, Error> {
    change(db, user, id, user_key::db::invalidate).await
}
