//! The instance's settings over the `setting` model: every key listed (a
//! secret one only as set or not), one written (checked by its
//! declaration, then logged), and the public ones by key.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    db::Db,
    error::Error,
    events,
    models::setting::{self, Listed, Setting},
    usecase::now,
};

/// Every setting, in declaration order; a secret one only as set or not.
///
/// # Errors
///
/// The store.
pub(crate) async fn list(db: &Db) -> Result<Vec<Listed>, Error> {
    Ok(db.read(setting::list).await?)
}

/// The public settings' values, by key (the default when never written).
///
/// # Errors
///
/// The store.
pub(crate) async fn public(db: &Db) -> Result<BTreeMap<String, Value>, Error> {
    let settings = list(db).await?.into_iter();
    let public = settings.filter_map(|listed| match listed {
        Listed::Value(setting) if setting.public => Some((setting.key, setting.value)),
        _ => None,
    });
    Ok(public.collect())
}

/// Writes `value` as `key`'s: the setting as written. Logged, with the
/// value only when the setting is public.
///
/// # Errors
///
/// Invalid for an unknown key, a wrong type or a failed check; permission
/// denied for a secret key; the store.
pub(crate) async fn write(db: &Db, key: &str, value: Value) -> Result<Setting, Error> {
    let (key, at) = (key.to_owned(), now());
    let written = db
        .write(move |connection| Ok(setting::write(connection, &key, value, at)))
        .await??;
    if written.public {
        tracing::info!(target: events::SETTING_CHANGED, key = written.key, value = %written.value);
    } else {
        tracing::info!(target: events::SETTING_CHANGED, key = written.key);
    }
    Ok(written)
}
