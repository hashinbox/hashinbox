//! The nouns the server uses across its endpoints and its storage, one file
//! per noun, and one file per table of the schema (`schema/*.sql`): the diesel
//! row, every statement that reads or writes it, and its conversions to and
//! from `base`'s types. The storage traits are implemented on these in
//! `crate::storage`.
//!
//! Each file reads top to bottom: the structs a model embeds, the model and
//! its constructors, its collection, then `db`, the table's statements
//! (`wallet::db::get_by_id(connection, id)`). A model names no HTTP
//! framework and no chain. A statement's first parameter is the connection
//! it runs on; the caller owns the transaction. Rows hold what the file holds (SQLite's
//! signed integers, exact amount text, raw hash bytes); conversions that can
//! refuse return `anyhow` errors that never quote a stored secret.

/// A model's collection: its rows in order, read as a slice, owned by
/// iteration, built from a `Vec` or an iterator.
macro_rules! collection {
    ($(#[$meta:meta])* $name:ident($model:ty)) => {
        $(#[$meta])*
        pub(crate) struct $name(Vec<$model>);

        impl std::ops::Deref for $name {
            type Target = [$model];

            fn deref(&self) -> &[$model] {
                &self.0
            }
        }

        impl IntoIterator for $name {
            type Item = $model;
            type IntoIter = std::vec::IntoIter<$model>;

            fn into_iter(self) -> Self::IntoIter {
                self.0.into_iter()
            }
        }

        impl From<Vec<$model>> for $name {
            fn from(rows: Vec<$model>) -> Self {
                Self(rows)
            }
        }

        impl FromIterator<$model> for $name {
            fn from_iter<I: IntoIterator<Item = $model>>(rows: I) -> Self {
                Self(rows.into_iter().collect())
            }
        }
    };
}

mod asset;
pub(crate) mod balance;
pub(crate) mod block;
pub(crate) mod endpoint;
mod indexing;
mod json;
pub(crate) mod leg;
mod network;
pub(crate) mod schema;
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "playground: wired by the pipe evaluator later")
)]
pub(crate) mod tag;
pub(crate) mod task;
pub(crate) mod transaction;
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "playground: wired by the pipe evaluator later")
)]
pub(crate) mod trigger;
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "playground: wired by the pipe evaluator later")
)]
pub(crate) mod trigger_run;
pub(crate) mod wallet;
pub(crate) mod wallet_index;
pub(crate) mod wallet_index_balance;

pub(crate) use asset::Asset;
pub(crate) use block::Block;
pub(crate) use endpoint::RpcEndpoints;
pub(crate) use indexing::Indexing;
pub(crate) use json::Json;
pub(crate) use leg::{Leg, Legs};
pub(crate) use network::Network;
#[cfg_attr(
    not(test),
    allow(
        unused_imports,
        reason = "playground: wired by the pipe evaluator later"
    )
)]
pub(crate) use tag::Tag;
pub(crate) use task::Task;

#[cfg_attr(
    not(test),
    allow(
        unused_imports,
        reason = "playground: wired by the pipe evaluator later"
    )
)]
pub(crate) use trigger::Trigger;
#[cfg_attr(
    not(test),
    allow(
        unused_imports,
        reason = "playground: wired by the pipe evaluator later"
    )
)]
pub(crate) use trigger_run::TriggerRun;
pub(crate) use wallet::Wallet;

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
