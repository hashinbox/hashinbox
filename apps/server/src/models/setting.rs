//! `setting`: the instance's settings, one row per key written; a key with
//! no row is its default. Every key is declared here, in code, by
//! [`settings!`]: its description, type, default, visibility (public,
//! private or secret) and its check. A row's `public` mirrors the
//! declaration on each write and is never taken from a request.

use std::collections::BTreeMap;

use diesel::prelude::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::schema::setting;
use crate::error::Error;

/// Declares the settings, each as
///
/// ```text
/// /// Its description (every doc line, joined).
/// name: Type = default, public | private | secret, valid = |value: &Type| -> bool,
/// ```
///
/// and generates, from them:
/// - `name(connection)`: the key's typed value, its default when no row is
///   kept (the doc comment is the function's);
/// - [`write`]: a key's JSON value parsed into its type and checked by its
///   `valid`, then kept; an unknown key, a wrong type or a failed check is
///   [`Error::invalid_argument`], a secret key [`Error::permission_denied`];
/// - [`put`]: the same for server code, a secret key too;
/// - [`list`]: every key, as a [`Listed`].
///
/// `Type` is (de)serialized by serde. `public` means its value may be sent
/// in any response, to anyone (`GET /v1/version`); `private`, only to a
/// caller holding `settings.$read`; `secret`, never: listed only as set or
/// not, and written by server code alone ([`put`]). A key's name must
/// not be `write`, `create`, `list` or `declared`.
macro_rules! settings {
    (@visibility public) => { Visibility::Public };
    (@visibility private) => { Visibility::Private };
    (@visibility secret) => { Visibility::Secret };
    (
        $(
            $(#[doc = $doc:literal])+
            $key:ident: $type:ty = $default:expr, $visibility:ident, valid = $valid:expr
        ),+ $(,)?
    ) => {
        $(
            $(#[doc = $doc])+
            ///
            /// Its default when no row is kept.
            ///
            /// # Errors
            ///
            /// The read, or a kept value that is not its type.
            #[allow(dead_code, reason = "a reader is there for each key, used or not")]
            pub(crate) fn $key(connection: &mut SqliteConnection) -> anyhow::Result<$type> {
                let kept = db::find(connection, stringify!($key))?;
                value_of(stringify!($key), kept.as_deref(), || $default)
            }
        )+

        /// `value` as `key`'s, once it parses as the key's type and passes
        /// its check: the setting it makes, and the key's visibility.
        ///
        /// # Errors
        ///
        /// [`Error::invalid_argument`] for an unknown key, a wrong type or
        /// a failed check.
        fn declared(key: &str, value: Value) -> Result<(Setting, Visibility), Error> {
            $(
                if key == stringify!($key) {
                    let description = description(&[$($doc),+]);
                    let typed: $type = serde_json::from_value(value).map_err(|error| {
                        Error::invalid_argument(format!(
                            "setting {key} must be a {}: {error}",
                            stringify!($type)
                        ))
                    })?;
                    let valid: fn(&$type) -> bool = $valid;
                    if !valid(&typed) {
                        return Err(Error::invalid_argument(format!(
                            "setting {key}: the value is refused. {description}"
                        )));
                    }
                    let visibility = settings!(@visibility $visibility);
                    let setting = Setting {
                        key: key.to_owned(),
                        value: serde_json::to_value(&typed).map_err(anyhow::Error::from)?,
                        default: serde_json::to_value::<$type>($default)
                            .map_err(anyhow::Error::from)?,
                        public: visibility == Visibility::Public,
                        description,
                        type_name: stringify!($type).to_owned(),
                    };
                    return Ok((setting, visibility));
                }
            )+
            Err(Error::invalid_argument(format!("no setting {key}")))
        }

        /// Every key, in declaration order: a public or private one with
        /// its value (its default when no row is kept), a secret one only
        /// as set or not; one read.
        ///
        /// # Errors
        ///
        /// The read, or a kept value that is not its key's type.
        pub(crate) fn list(connection: &mut SqliteConnection) -> anyhow::Result<Vec<Listed>> {
            let kept = db::find_all(connection)?;
            Ok(vec![$({
                let (key, visibility) = (stringify!($key), settings!(@visibility $visibility));
                let description = description(&[$($doc),+]);
                if visibility == Visibility::Secret {
                    Listed::Secret(SecretSetting {
                        key: key.to_owned(),
                        set: kept.contains_key(key),
                        description,
                    })
                } else {
                    Listed::Value(Setting {
                        key: key.to_owned(),
                        value: serde_json::to_value(value_of::<$type>(
                            key,
                            kept.get(key).map(String::as_str),
                            || $default,
                        )?)?,
                        default: serde_json::to_value::<$type>($default)?,
                        public: visibility == Visibility::Public,
                        description,
                        type_name: stringify!($type).to_owned(),
                    })
                }
            },)+])
        }
    };
}

/// Keeps `value` as `key`'s at `now` (Unix ms), once it parses as the
/// key's type and passes its check: what `PUT /v1/settings/{key}` writes.
/// The setting as written.
///
/// # Errors
///
/// [`Error::invalid_argument`] for an unknown key, a wrong type or a failed
/// check; [`Error::permission_denied`] for a secret key (only server code
/// sets one, [`put`]); the store.
pub(crate) fn write(
    connection: &mut SqliteConnection,
    key: &str,
    value: Value,
    now: i64,
) -> Result<Setting, Error> {
    let (setting, visibility) = declared(key, value)?;
    if visibility == Visibility::Secret {
        return Err(Error::permission_denied(format!(
            "setting {key} is secret: only the server sets it"
        )));
    }
    let value = setting.value.to_string();
    db::replace(connection, key, &value, setting.public, now).map_err(anyhow::Error::from)?;
    Ok(setting)
}

/// Keeps `value` as `key`'s at `now` (Unix ms), checked as [`write`]
/// checks it, replacing the row when there is one: server code's writer, a
/// secret key too. Whether it replaced one.
///
/// # Errors
///
/// [`Error::invalid_argument`] for an unknown key, a wrong type or a failed
/// check; the store.
pub(crate) fn put(
    connection: &mut SqliteConnection,
    key: &str,
    value: Value,
    now: i64,
) -> Result<bool, Error> {
    let (setting, _) = declared(key, value)?;
    let value = setting.value.to_string();
    let replaced = db::find(connection, key)
        .map_err(anyhow::Error::from)?
        .is_some();
    db::replace(connection, key, &value, setting.public, now).map_err(anyhow::Error::from)?;
    Ok(replaced)
}

/// The recovery passphrase's key (design 096).
pub(crate) const RECOVERY_PASSPHRASE: &str = "recovery_passphrase";

settings! {
    /// The instance's name: shown in the top-left corner of the dashboard and its page title.
    instance: String = "hashinbox".to_owned(), public, valid = |name: &String| (1..=64).contains(&name.chars().count()),
    /// The argon2id hash of the recovery passphrase (12 BIP39 words, never kept): it signs in as any user. Optional: the root creates, replaces or removes it (`/v1/recovery`).
    recovery_passphrase: Option<String> = None, secret, valid = |hash: &Option<String>| hash.as_deref().is_some_and(|hash| hash.starts_with("$argon2id$")),
}

/// Who may see a setting's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    /// Anyone, in any response (`GET /v1/version`).
    Public,
    /// A caller holding `settings.$read`.
    #[allow(dead_code, reason = "no private setting is declared yet")]
    Private,
    /// No one: listed only as set or not, set by server code alone.
    Secret,
}

/// A setting as listed: its value, or for a secret one only whether it is
/// set.
#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub(crate) enum Listed {
    /// A public or private setting, its value shown.
    Value(Setting),
    /// A secret setting: never its value.
    Secret(SecretSetting),
}

/// A secret setting as listed: never its value or default.
#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct SecretSetting {
    /// Its name: `recovery_passphrase`.
    pub(crate) key: String,
    /// Whether it has been set.
    pub(crate) set: bool,
    /// What it is for: its declaration's doc comment.
    pub(crate) description: String,
}

/// A setting as listed: its key, value, default and declaration.
#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct Setting {
    /// Its name: `instance`.
    pub(crate) key: String,
    /// Its value, JSON of its type; the default when never written.
    pub(crate) value: Value,
    /// Its default, JSON of its type.
    pub(crate) default: Value,
    /// Whether its value may be sent in any response, to anyone.
    pub(crate) public: bool,
    /// What it is for: its declaration's doc comment.
    pub(crate) description: String,
    /// Its Rust type, as declared: `String`.
    #[serde(rename = "type")]
    pub(crate) type_name: String,
}

/// A declaration's doc lines, joined by spaces.
fn description(lines: &[&str]) -> String {
    let lines = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty());
    lines.collect::<Vec<_>>().join(" ")
}

/// `kept` (key `key`'s stored JSON) as its type; `default()` when `None`.
///
/// # Errors
///
/// When the kept JSON is not that type.
fn value_of<T: DeserializeOwned>(
    key: &str,
    kept: Option<&str>,
    default: impl FnOnce() -> T,
) -> anyhow::Result<T> {
    use anyhow::Context as _;
    kept.map_or_else(
        || Ok(default()),
        |kept| {
            serde_json::from_str(kept)
                .with_context(|| format!("kept setting {key} is not its type"))
        },
    )
}

/// The `setting` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Key `key`'s kept JSON, if written.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        key: &str,
    ) -> QueryResult<Option<String>> {
        setting::table
            .find(key)
            .select(setting::value)
            .first(connection)
            .optional()
    }

    /// Every kept JSON, by key.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
    ) -> QueryResult<BTreeMap<String, String>> {
        let rows = setting::table
            .select((setting::key, setting::value))
            .load::<(String, String)>(connection)?;
        Ok(rows.into_iter().collect())
    }

    /// When key `key` was last written (Unix ms), if it is kept.
    pub(crate) fn updated_at(
        connection: &mut SqliteConnection,
        key: &str,
    ) -> QueryResult<Option<i64>> {
        setting::table
            .find(key)
            .select(setting::updated_at)
            .first(connection)
            .optional()
    }

    /// Drops key `key`'s row (its default again): whether one was kept.
    pub(crate) fn delete(connection: &mut SqliteConnection, key: &str) -> QueryResult<bool> {
        diesel::delete(setting::table.find(key))
            .execute(connection)
            .map(|deleted| deleted == 1)
    }

    /// Keeps `value` (JSON) as key `key`'s at `now` (Unix ms), `public` as
    /// declared; replaces the row when there is one.
    pub(crate) fn replace(
        connection: &mut SqliteConnection,
        key: &str,
        value: &str,
        public: bool,
        now: i64,
    ) -> QueryResult<()> {
        diesel::replace_into(setting::table)
            .values((
                setting::key.eq(key),
                setting::value.eq(value),
                setting::public.eq(public),
                setting::updated_at.eq(now),
            ))
            .execute(connection)
            .map(drop)
    }
}
