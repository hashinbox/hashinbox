//! `user`: a person or team owner (design 096), the widest scopes anything
//! they issue (keys, sessions) may have; read by id on every
//! authenticated request.

use diesel::prelude::*;
use scope::Scopes;
use serde::Serialize;

use super::{
    Session, UserKey,
    json::{PackedScopes, serialize_is_some, serialize_packed},
    schema::users,
};
use crate::{auth, db::Page, error::Error};

/// The fewest characters a password has.
const MIN_PASSWORD: usize = 8;

/// Checks `password` as every new one is checked (at signup, on a
/// change): 8 characters at least.
///
/// # Errors
///
/// [`Error::invalid_argument`] when it is shorter.
pub(crate) fn check_password(password: &str) -> Result<(), Error> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(Error::invalid_argument(format!(
            "a password needs {MIN_PASSWORD} characters at least"
        )));
    }
    Ok(())
}

/// A user's id: a uuid v7. Its client id is `user/{id}`.
pub(crate) type UserId = String;

/// One `user` row. Its JSON shows neither the password hash (only whether
/// it has one, `login`) nor the session secret.
#[derive(Clone, Debug, Queryable, Selectable, Serialize, utoipa::ToSchema)]
#[diesel(table_name = users)]
pub(crate) struct User {
    /// A uuid v7.
    pub(crate) id: UserId,
    /// Its login, unique.
    pub(crate) email: String,
    /// Its name, as shown.
    pub(crate) name: String,
    /// The argon2id hash of its password; `None`: no dashboard login. In
    /// JSON, `login`: whether it has one.
    #[serde(rename = "login", serialize_with = "serialize_is_some")]
    #[schema(value_type = bool)]
    pub(crate) password_hash: Option<String>,
    /// The widest scopes it may issue, packed in JSON
    /// (`{"com.hashinbox": ["wallets:read"]}`).
    #[diesel(deserialize_as = PackedScopes)]
    #[serde(serialize_with = "serialize_packed")]
    #[schema(value_type = std::collections::BTreeMap<String, scope::Group>)]
    pub(crate) scopes: Scopes,
    /// 32 random bytes signing its sessions (HS256); never shown.
    #[serde(skip)]
    pub(crate) session_secret: Vec<u8>,
    /// Unix ms.
    pub(crate) created_at: i64,
    /// Unix ms; every session issued in its second or before is refused.
    pub(crate) invalidated_at: Option<i64>,
    /// Unix ms it was disabled: it, its keys and every session refused.
    pub(crate) disabled_at: Option<i64>,
}

impl User {
    /// A new user, created at `now` (Unix ms): a uuid v7 id and a new
    /// session secret.
    ///
    /// # Errors
    ///
    /// When the OS gives no random bytes.
    pub(crate) fn new(
        email: String,
        name: String,
        password_hash: Option<String>,
        scopes: Scopes,
        now: i64,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            id: uuid::Uuid::now_v7().to_string(),
            email,
            name,
            password_hash,
            scopes,
            session_secret: crypto::random::<32>()?.to_vec(),
            created_at: now,
            invalidated_at: None,
            disabled_at: None,
        })
    }

    /// Whether it lets `session` (its own, or one of `key`'s) call: not
    /// disabled, not invalidated since the session was issued, not ended by
    /// the recovery passphrase (set at `recovery_set_at`, Unix ms), and its
    /// scopes covering the session's and the key's, each expanded
    /// (`auth::DEPENDS`).
    pub(crate) fn accepts(
        &self,
        session: &Session,
        key: Option<&UserKey>,
        recovery_set_at: Option<i64>,
    ) -> bool {
        let held = auth::expanded(&self.scopes);
        let covers = |scopes: &Scopes| held.first_uncovered(&auth::expanded(scopes)).is_none();
        self.disabled_at.is_none()
            && !session.is_invalidated_by(self.invalidated_at)
            && !session.is_ended_by_recovery(recovery_set_at)
            && covers(&session.scopes)
            && key.is_none_or(|key| covers(&key.scopes))
    }
}

/// A page's key: its id.
impl crate::db::Keyed for User {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// The `user` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Whether any user is kept (`SELECT EXISTS(SELECT 1 FROM "user")`).
    pub(crate) fn any(connection: &mut SqliteConnection) -> QueryResult<bool> {
        diesel::select(diesel::dsl::exists(users::table.select(users::id))).get_result(connection)
    }

    /// User `id`, if kept.
    pub(crate) fn find(connection: &mut SqliteConnection, id: &str) -> QueryResult<Option<User>> {
        users::table
            .find(id)
            .select(User::as_select())
            .first(connection)
            .optional()
    }

    /// The user whose login is `email`, if kept.
    pub(crate) fn find_by_email(
        connection: &mut SqliteConnection,
        email: &str,
    ) -> QueryResult<Option<User>> {
        users::table
            .filter(users::email.eq(email))
            .select(User::as_select())
            .first(connection)
            .optional()
    }

    /// Keeps the new `user`.
    ///
    /// # Errors
    ///
    /// A unique violation when its email is taken.
    pub(crate) fn insert(connection: &mut SqliteConnection, user: &User) -> QueryResult<()> {
        diesel::insert_into(users::table)
            .values((
                users::id.eq(&user.id),
                users::email.eq(&user.email),
                users::name.eq(&user.name),
                users::password_hash.eq(&user.password_hash),
                users::scopes.eq(PackedScopes(user.scopes.clone())),
                users::session_secret.eq(&user.session_secret),
                users::created_at.eq(user.created_at),
            ))
            .execute(connection)
            .map(drop)
    }

    /// At most `page.limit` users with an id after `page.after`, by id.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        page: Page<String>,
    ) -> QueryResult<Vec<User>> {
        users::table
            .filter(users::id.gt(page.after.unwrap_or_default()))
            .order(users::id)
            .limit(page.limit)
            .select(User::as_select())
            .load(connection)
    }

    /// Sets user `id`'s scopes; how many (0: no such user).
    pub(crate) fn set_scopes(
        connection: &mut SqliteConnection,
        id: &str,
        scopes: &Scopes,
    ) -> QueryResult<usize> {
        diesel::update(users::table.find(id))
            .set(users::scopes.eq(PackedScopes(scopes.clone())))
            .execute(connection)
    }

    /// Sets user `id`'s `disabled_at` (`None`: enabled); how many.
    pub(crate) fn set_disabled_at(
        connection: &mut SqliteConnection,
        id: &str,
        at: Option<i64>,
    ) -> QueryResult<usize> {
        diesel::update(users::table.find(id))
            .set(users::disabled_at.eq(at))
            .execute(connection)
    }

    /// Sets user `id`'s password hash, invalidating its sessions at `at`;
    /// how many.
    pub(crate) fn set_password_hash(
        connection: &mut SqliteConnection,
        id: &str,
        hash: &str,
        at: i64,
    ) -> QueryResult<usize> {
        diesel::update(users::table.find(id))
            .set((users::password_hash.eq(hash), users::invalidated_at.eq(at)))
            .execute(connection)
    }

    /// Invalidates every session user `id` and its keys issued up to `at`
    /// (Unix ms); how many.
    pub(crate) fn invalidate(
        connection: &mut SqliteConnection,
        id: &str,
        at: i64,
    ) -> QueryResult<usize> {
        diesel::update(users::table.find(id))
            .set(users::invalidated_at.eq(at))
            .execute(connection)
    }
}
