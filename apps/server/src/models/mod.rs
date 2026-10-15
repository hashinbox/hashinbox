//! The nouns the server uses across its endpoints and its storage, one file
//! per noun, and one file per table of the schema (`schema/*.sql`): the diesel
//! row, every statement that reads or writes it, and its conversions to and
//! from `base`'s types. The storage traits are implemented on these in
//! `crate::storage`.
//!
//! A model names no HTTP framework and no chain. Its statements are
//! associated functions whose first parameter is the connection they run on;
//! the caller owns the transaction. Rows hold what the file holds (SQLite's
//! signed integers, exact amount text, raw hash bytes); conversions that can
//! refuse return `anyhow` errors that never quote a stored secret.

mod asset;
mod block;
mod endpoint;
mod indexing;
mod leg;
mod network;
pub(crate) mod schema;
mod task;
mod transaction;
mod wallet;
mod wallet_index;

pub(crate) use asset::Asset;
pub(crate) use block::Block;
pub(crate) use endpoint::{Endpoint, NetworkEndpoints, RpcEndpoints};
pub(crate) use indexing::Indexing;
pub(crate) use leg::{LegFacts, LegRow};
pub(crate) use network::Network;
pub(crate) use task::Task;
pub(crate) use transaction::TransactionRow;
pub(crate) use wallet::WalletRow;
pub(crate) use wallet_index::WalletIndexRow;

use anyhow::{Context as _, ensure};

/// `value` as a SQLite integer.
///
/// # Errors
///
/// When it is above `i64::MAX`.
pub(crate) fn int(value: u64) -> anyhow::Result<i64> {
    i64::try_from(value).context("a value is above SQLite's integer range")
}

/// A stored SQLite integer as the unsigned value it was written from.
///
/// # Errors
///
/// When it is negative, which no writer stores.
pub(crate) fn uint(value: i64) -> anyhow::Result<u64> {
    u64::try_from(value).context("a stored value is negative")
}

/// A row id: `parts` joined with dots (see `schema/sdk.sql`).
///
/// # Errors
///
/// When a part is empty or holds a dot, so the id would not split back.
pub(crate) fn key(parts: &[&str]) -> anyhow::Result<String> {
    for part in parts {
        ensure!(
            !part.is_empty() && !part.contains('.'),
            "an id part is empty or holds a dot: {part:?}"
        );
    }
    Ok(parts.join("."))
}

/// The id of a row under row `parent` (an id [`key`] built): `parent` and
/// `parts` joined with dots.
///
/// # Errors
///
/// When a part is empty or holds a dot.
pub(crate) fn child(parent: &str, parts: &[&str]) -> anyhow::Result<String> {
    Ok(format!("{parent}.{}", key(parts)?))
}
