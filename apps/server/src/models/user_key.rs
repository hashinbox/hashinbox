//! `user_key`: a key a user issued (design 096), its public client id
//! (`hip/…`), its secret (`his/…`, kept readable: the HS256 key that
//! checks the sessions it signs) and the secret's SHA-256 (finds the key
//! from a bearer request). Read by id or secret hash on every request a
//! key makes. Both the id and the secret are base 58 (`crypto::base58`):
//! no `0`, `O`, `I` or `l`, and no character a URL or a double-click splits.

use crypto::{base58, crc32, random, sha256};
use diesel::prelude::*;
use scope::Scopes;
use serde::Serialize;

use super::{
    Session,
    json::{PackedScopes, serialize_packed},
    schema::user_key,
};
use crate::{auth, db::Page};

/// The prefix of a key's client id.
const ID_PREFIX: &str = "hip/";
/// The prefix of a key's secret: what secret scanners match.
pub(crate) const SECRET_PREFIX: &str = "his/";
/// The secret's random bytes.
const RANDOM_BYTES: usize = 32;

/// A new key's client id: `hip/` and 128 random bits in base 58.
///
/// # Errors
///
/// When the OS gives no random bytes.
pub(crate) fn new_id() -> anyhow::Result<String> {
    Ok(format!("{ID_PREFIX}{}", base58::encode(&random::<16>()?)))
}

/// A key's secret: `his/` then, in one base-58 number, 32 random bytes and
/// their CRC32 (4 bytes, big-endian), our typo check refusing a mistyped
/// secret before any lookup. Its `Debug` shows nothing of it.
pub(crate) struct KeySecret(String);

impl KeySecret {
    /// A new random secret.
    ///
    /// # Errors
    ///
    /// When the OS gives no random bytes.
    pub(crate) fn generate() -> anyhow::Result<Self> {
        let mut bytes = random::<RANDOM_BYTES>()?.to_vec();
        bytes.extend(crc32(&bytes).to_be_bytes());
        Ok(Self(format!("{SECRET_PREFIX}{}", base58::encode(&bytes))))
    }

    /// The secret `text` is; `None` when it is not one, or its checksum
    /// does not match.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let bytes = base58::decode(text.strip_prefix(SECRET_PREFIX)?)?;
        if bytes.len() != RANDOM_BYTES + 4 {
            return None;
        }
        let (random, crc) = bytes.split_at(RANDOM_BYTES);
        (crc32(random).to_be_bytes() == crc).then(|| Self(text.to_owned()))
    }

    /// Its text, `his/…`: shown once, when the key is created; kept, and
    /// the HS256 key of the key's sessions as it is.
    pub(crate) fn reveal(&self) -> &str {
        &self.0
    }

    /// The SHA-256 of its text: how a bearer request finds its key.
    pub(crate) fn hash(&self) -> [u8; 32] {
        sha256(self.0.as_bytes())
    }
}

impl std::fmt::Debug for KeySecret {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("KeySecret(…)")
    }
}

/// A key's id, its public client id: `hip/…`.
pub(crate) type UserKeyId = String;

/// One `user_key` row. Its JSON shows neither the secret nor its hash.
#[derive(Clone, Debug, Queryable, Selectable, Serialize, utoipa::ToSchema)]
#[diesel(table_name = user_key)]
pub(crate) struct UserKey {
    /// `hip/…`: its public client id, a session's `kid` and `iss`.
    pub(crate) id: UserKeyId,
    /// The user that issued it.
    pub(crate) user_id: String,
    /// What it is for: `shop backend`.
    pub(crate) name: String,
    /// SHA-256 of its secret's text.
    #[serde(skip)]
    pub(crate) secret_hash: Vec<u8>,
    /// Its secret's text, `his/…`: the HS256 key of its sessions.
    #[serde(skip)]
    pub(crate) secret: String,
    /// What it may do, covered by its user's; packed in JSON.
    #[diesel(deserialize_as = PackedScopes)]
    #[serde(serialize_with = "serialize_packed")]
    #[schema(value_type = std::collections::BTreeMap<String, scope::Group>)]
    pub(crate) scopes: Scopes,
    /// Unix ms.
    pub(crate) created_at: i64,
    /// Unix ms it stops working; `None`: never.
    pub(crate) expires_at: Option<i64>,
    /// Unix ms; every session it issued in that second or before is
    /// refused (rotated).
    pub(crate) invalidated_at: Option<i64>,
    /// Unix ms it was revoked: it and its sessions refused for good.
    pub(crate) revoked_at: Option<i64>,
    /// Unix ms it was last used; not kept yet.
    pub(crate) last_used_at: Option<i64>,
}

impl UserKey {
    /// A new key of user `user_id`, created at `now` (Unix ms), checked by
    /// `secret` and its hash.
    ///
    /// # Errors
    ///
    /// When the OS gives no random bytes.
    pub(crate) fn new(
        user_id: String,
        name: String,
        scopes: Scopes,
        expires_at: Option<i64>,
        secret: &KeySecret,
        now: i64,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            id: new_id()?,
            user_id,
            name,
            secret_hash: secret.hash().to_vec(),
            secret: secret.reveal().to_owned(),
            scopes,
            created_at: now,
            expires_at,
            invalidated_at: None,
            revoked_at: None,
            last_used_at: None,
        })
    }

    /// Whether it lets `session` (one it issued, or its bearer secret's)
    /// call at `now` (Unix ms): not revoked or expired, not invalidated
    /// since the session was issued, and its scopes covering the session's,
    /// each expanded (`auth::DEPENDS`).
    pub(crate) fn accepts(&self, session: &Session, now: i64) -> bool {
        self.revoked_at.is_none()
            && self.expires_at.is_none_or(|expires| expires > now)
            && !session.is_invalidated_by(self.invalidated_at)
            && auth::expanded(&self.scopes)
                .first_uncovered(&auth::expanded(&session.scopes))
                .is_none()
    }
}

/// A page's key: its id.
impl crate::db::Keyed for UserKey {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// The `user_key` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Key `id`, if kept.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> QueryResult<Option<UserKey>> {
        user_key::table
            .find(id)
            .select(UserKey::as_select())
            .first(connection)
            .optional()
    }

    /// The key whose secret's SHA-256 is `hash` (a bearer request), if
    /// kept.
    pub(crate) fn find_by_secret_hash(
        connection: &mut SqliteConnection,
        hash: &[u8],
    ) -> QueryResult<Option<UserKey>> {
        user_key::table
            .filter(user_key::secret_hash.eq(hash))
            .select(UserKey::as_select())
            .first(connection)
            .optional()
    }

    /// Keeps the new `key`.
    pub(crate) fn insert(connection: &mut SqliteConnection, key: &UserKey) -> QueryResult<()> {
        diesel::insert_into(user_key::table)
            .values((
                user_key::id.eq(&key.id),
                user_key::user_id.eq(&key.user_id),
                user_key::name.eq(&key.name),
                user_key::secret_hash.eq(&key.secret_hash),
                user_key::secret.eq(&key.secret),
                user_key::scopes.eq(PackedScopes(key.scopes.clone())),
                user_key::created_at.eq(key.created_at),
                user_key::expires_at.eq(key.expires_at),
            ))
            .execute(connection)
            .map(drop)
    }

    /// At most `page.limit` of user `user_id`'s keys with an id after
    /// `page.after`, by id.
    pub(crate) fn find_page_of_user(
        connection: &mut SqliteConnection,
        user_id: &str,
        page: Page<String>,
    ) -> QueryResult<Vec<UserKey>> {
        user_key::table
            .filter(user_key::user_id.eq(user_id))
            .filter(user_key::id.gt(page.after.unwrap_or_default()))
            .order(user_key::id)
            .limit(page.limit)
            .select(UserKey::as_select())
            .load(connection)
    }

    /// Revokes user `user_id`'s key `id` at `at` (Unix ms), if not
    /// revoked yet; how many.
    pub(crate) fn revoke(
        connection: &mut SqliteConnection,
        user_id: &str,
        id: &str,
        at: i64,
    ) -> QueryResult<usize> {
        diesel::update(user_key::table.find(id))
            .filter(user_key::user_id.eq(user_id))
            .filter(user_key::revoked_at.is_null())
            .set(user_key::revoked_at.eq(at))
            .execute(connection)
    }

    /// Invalidates every session user `user_id`'s key `id` issued up to
    /// `at` (Unix ms); how many.
    pub(crate) fn invalidate(
        connection: &mut SqliteConnection,
        user_id: &str,
        id: &str,
        at: i64,
    ) -> QueryResult<usize> {
        diesel::update(user_key::table.find(id))
            .filter(user_key::user_id.eq(user_id))
            .set(user_key::invalidated_at.eq(at))
            .execute(connection)
    }
}

#[cfg(test)]
mod tests {
    use super::{KeySecret, new_id};

    #[test]
    fn a_secret_round_trips_and_refuses_a_typo() {
        let secret = KeySecret::generate().unwrap();
        let text = secret.reveal().to_owned();
        assert!(text.starts_with("his/") && text.len() <= 4 + 50, "{text}");
        let parsed = KeySecret::parse(&text).unwrap();
        assert_eq!(parsed.reveal(), text);
        assert_eq!(parsed.hash(), secret.hash());
        let mut typo = text.into_bytes();
        typo[10] = if typo[10] == b'a' { b'b' } else { b'a' };
        assert!(KeySecret::parse(&String::from_utf8(typo).unwrap()).is_none());
        assert!(KeySecret::parse("his/short").is_none());
        assert!(KeySecret::parse("his/0OIl").is_none(), "not base 58");
        assert_eq!(format!("{secret:?}"), "KeySecret(…)");
    }

    #[test]
    fn an_id_is_hip_and_base58() {
        let id = new_id().unwrap();
        let digits = id.strip_prefix("hip/").unwrap();
        assert_eq!(
            crypto::base58::decode(digits).map(|bytes| bytes.len()),
            Some(16),
            "{id}"
        );
    }
}
