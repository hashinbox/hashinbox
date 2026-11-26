//! `user_2fa`: a user's TOTP (RFC 6238, `crypto::totp`): on once its first
//! code is confirmed, each code used once (`last_step`).

use diesel::prelude::*;

use crypto::totp;

use super::schema::user_2fa;

/// One `user_2fa` row.
#[derive(Clone, Debug, Queryable, Selectable)]
#[diesel(table_name = user_2fa)]
pub(crate) struct User2fa {
    /// Its user.
    pub(crate) user_id: String,
    /// The TOTP secret, 20 bytes, kept plain.
    pub(crate) secret: Vec<u8>,
    /// Unix ms its first code was entered; `None`: not on yet.
    pub(crate) confirmed_at: Option<i64>,
    /// The step of the last code used.
    pub(crate) last_step: Option<i64>,
    /// Unix ms.
    pub(crate) created_at: i64,
}

impl User2fa {
    /// The step `code` is at `now` (Unix ms), not used before; `None`
    /// when it is wrong or replayed.
    pub(crate) fn step_of(&self, code: &str, now: i64) -> Option<i64> {
        totp::verify(&self.secret, code, now.div_euclid(1000), self.last_step)
    }
}

/// The `user_2fa` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// User `user_id`'s TOTP, if any.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        user_id: &str,
    ) -> QueryResult<Option<User2fa>> {
        user_2fa::table
            .find(user_id)
            .select(User2fa::as_select())
            .first(connection)
            .optional()
    }

    /// Keeps `row`, replacing its user's unconfirmed one.
    pub(crate) fn replace(connection: &mut SqliteConnection, row: &User2fa) -> QueryResult<()> {
        diesel::replace_into(user_2fa::table)
            .values((
                user_2fa::user_id.eq(&row.user_id),
                user_2fa::secret.eq(&row.secret),
                user_2fa::confirmed_at.eq(row.confirmed_at),
                user_2fa::last_step.eq(row.last_step),
                user_2fa::created_at.eq(row.created_at),
            ))
            .execute(connection)
            .map(drop)
    }

    /// Records that user `user_id` used the code of `step`, confirming it
    /// at `at` (Unix ms) if not yet; how many.
    pub(crate) fn use_step(
        connection: &mut SqliteConnection,
        user_id: &str,
        step: i64,
        at: i64,
    ) -> QueryResult<usize> {
        diesel::update(user_2fa::table.find(user_id))
            .filter(user_2fa::confirmed_at.is_null())
            .set(user_2fa::confirmed_at.eq(at))
            .execute(connection)?;
        diesel::update(user_2fa::table.find(user_id))
            .set(user_2fa::last_step.eq(step))
            .execute(connection)
    }

    /// Deletes user `user_id`'s TOTP; how many.
    pub(crate) fn delete(connection: &mut SqliteConnection, user_id: &str) -> QueryResult<usize> {
        diesel::delete(user_2fa::table.find(user_id)).execute(connection)
    }
}
