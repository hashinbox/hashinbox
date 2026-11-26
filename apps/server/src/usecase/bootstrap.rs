//! The preset an empty database is seeded with (`--config`,
//! `HASHINBOX_CONFIG`): the root user and settings, in one write, each
//! checked as `POST /v1/users`, `PUT /v1/settings/{key}` and
//! `POST /v1/recovery` check them. Once a user exists it is not read. Its
//! content is never logged; its errors name the field, never the password
//! or the words.

use std::collections::BTreeMap;

use anyhow::{Context as _, anyhow, bail};
use crypto::mnemonic::Mnemonic;
use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::{Value, json};

use super::{
    now, recovery,
    users::{self, NewUser},
};
use crate::{
    db::Db,
    error::Error,
    events,
    models::{setting, user},
};

/// The preset: `{"root": {…}, "settings": {…}}`, both optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The root user, as `POST /v1/users` takes it.
    #[serde(default)]
    root: Option<Root>,
    /// Values by key, as `PUT /v1/settings/{key}` takes each;
    /// `recovery_passphrase` as its 12 BIP39 words.
    #[serde(default)]
    settings: BTreeMap<String, Value>,
}

/// The root user asked for.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    email: String,
    name: String,
    password: Secret,
}

/// A string whose wrong type is refused without echoing the value.
struct Secret(String);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(text) => Ok(Self(text)),
            _ => Err(D::Error::custom("must be a string")),
        }
    }
}

impl Config {
    /// `text` as a preset: inline JSON when it starts with `{` (trimmed),
    /// else a JSON file's path.
    fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let json = if text.starts_with('{') {
            text.to_owned()
        } else {
            std::fs::read_to_string(text).with_context(|| format!("config file {text}"))?
        };
        let mut json = serde_json::Deserializer::from_str(&json);
        serde_path_to_error::deserialize(&mut json).map_err(|error| anyhow!("config: {error}"))
    }
}

/// `error` of the preset's `field`: its message alone.
fn refused(field: &str, error: &Error) -> anyhow::Error {
    anyhow!("config {field}: {}", error.message)
}

/// Whether a user exists, once `config` (`--config`'s text) has seeded an
/// empty database: its root created and its settings kept in one write,
/// logged. With a user, `config` is not read; without
/// `config`, nothing changes.
///
/// # Errors
///
/// Any problem with `config`, naming the field; nothing is written then.
/// A config with no `root` while no user exists. The store.
pub(crate) async fn bootstrap(db: &Db, config: Option<&str>) -> anyhow::Result<bool> {
    let exists = db.read(|connection| Ok(user::db::any(connection)?)).await?;
    let Some(config) = config else {
        return Ok(exists);
    };
    if exists {
        return Ok(true);
    }
    let Config { root, mut settings } = Config::parse(config)?;
    let Some(root) = root else {
        bail!("config root: needed while no user exists (settings alone are not applied)");
    };
    let new = NewUser::new(&root.email, &root.name, Some(root.password.0), None)
        .map_err(|error| refused("root", &error))?;
    let root = users::root_of(new)
        .await
        .map_err(|error| refused("root", &error))?;
    let passphrase = format!("settings.{}", setting::RECOVERY_PASSPHRASE);
    let recovery = match settings.remove(setting::RECOVERY_PASSPHRASE) {
        None => None,
        Some(Value::String(words)) => {
            let mnemonic = Mnemonic::parse(&words)
                .map_err(|invalid| anyhow!("config {passphrase}: {invalid}"))?;
            let hash = recovery::hashed(&mnemonic).await;
            Some(hash.map_err(|error| refused(&passphrase, &error))?)
        }
        Some(_) => bail!("config {passphrase}: must be a string of 12 BIP39 words"),
    };
    let mut keys: Vec<String> = settings.keys().cloned().collect();
    if recovery.is_some() {
        keys.push(setting::RECOVERY_PASSPHRASE.to_owned());
    }
    let (email, at) = (root.email.clone(), now());
    db.write(move |connection| {
        users::insert_root(connection, &root)?.map_err(|error| refused("root", &error))?;
        for (key, value) in settings {
            setting::write(connection, &key, value, at)
                .map_err(|error| refused(&format!("settings.{key}"), &error))?;
        }
        if let Some(hash) = recovery {
            setting::put(connection, setting::RECOVERY_PASSPHRASE, json!(hash), at)
                .map_err(|error| refused(&passphrase, &error))?;
        }
        Ok(())
    })
    .await?;
    let settings = if keys.is_empty() {
        "none".to_owned()
    } else {
        keys.join(", ")
    };
    tracing::info!(target: events::APP_BOOTSTRAP, email, settings);
    Ok(true)
}
