//! The recovery passphrase (design 096): optional, 12 BIP39 words the root
//! creates, replaces or removes at any time, shown once, kept only as their
//! argon2id hash (the secret setting `recovery_passphrase`); they sign in
//! as any user (`sessions::login`). Replacing or removing it ends every
//! session its old words made (`Session::is_ended_by_recovery`).

use std::time::Duration;

use crypto::{mnemonic::Mnemonic, password};
use serde_json::json;

use super::now;
use crate::{db::Db, error::Error, events, models::setting};

/// When the passphrase was set (Unix ms), if one is.
///
/// # Errors
///
/// The store.
pub(crate) async fn set_at(db: &Db) -> Result<Option<i64>, Error> {
    Ok(db
        .read(|connection| {
            Ok(setting::db::updated_at(
                connection,
                setting::RECOVERY_PASSPHRASE,
            )?)
        })
        .await?)
}

/// `mnemonic`'s argon2id hash, of its normal form (what a login checks),
/// on a blocking thread: the value `recovery_passphrase` keeps.
///
/// # Errors
///
/// The hash; no randomness.
pub(crate) async fn hashed(mnemonic: &Mnemonic) -> Result<String, Error> {
    let words = mnemonic.to_string();
    let hash = tokio::task::spawn_blocking(move || password::hash(&words))
        .await
        .map_err(|_| Error::unavailable("the passphrase hash did not finish"))??;
    Ok(hash)
}

/// A new passphrase, its hash kept in place of any one set, made by user
/// `by`: the words, shown once. They are answered once the second they
/// were set in is over, so every session they make is issued after it
/// (the old words' sessions, issued in it or before, are ended).
///
/// # Errors
///
/// The store.
pub(crate) async fn create(db: &Db, by: &str) -> Result<Mnemonic, Error> {
    let mnemonic = Mnemonic::generate()?;
    let hash = hashed(&mnemonic).await?;
    let at = now();
    let replaced = db
        .write(move |connection| {
            Ok(setting::put(
                connection,
                setting::RECOVERY_PASSPHRASE,
                json!(hash),
                at,
            ))
        })
        .await??;
    if replaced {
        tracing::info!(target: events::AUTH_RECOVERY_REPLACED, user = by);
    } else {
        tracing::info!(target: events::AUTH_RECOVERY_CREATED, user = by);
    }
    let rest = u64::try_from(1000 - at.rem_euclid(1000)).unwrap_or_default();
    tokio::time::sleep(Duration::from_millis(rest)).await;
    Ok(mnemonic)
}

/// Removes the passphrase, by user `by`: passphrase logins are refused
/// like wrong words, and every session they made ends. Nothing when none
/// is set.
///
/// # Errors
///
/// The store.
pub(crate) async fn remove(db: &Db, by: &str) -> Result<(), Error> {
    let removed = db
        .write(|connection| {
            Ok(setting::db::delete(
                connection,
                setting::RECOVERY_PASSPHRASE,
            )?)
        })
        .await?;
    if removed {
        tracing::info!(target: events::AUTH_RECOVERY_REMOVED, user = by);
    }
    Ok(())
}
