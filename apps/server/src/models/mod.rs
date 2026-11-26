//! The nouns the server uses across its endpoints and its storage, one file
//! per noun, and one file per table of the schema (`init.sql`): the diesel
//! row, every statement that reads or writes it, and its conversions to and
//! from `base`'s types. The storage traits are implemented on these in
//! `crate::usecase::sdk` and `crate::usecase::tasks`.
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
pub(crate) mod block;
pub(crate) mod caller;
pub(crate) mod endpoint;
pub(crate) mod endpoint_failure;
pub(crate) mod endpoint_route;
pub(crate) mod endpoint_stats;
pub(crate) mod id;
mod json;
pub(crate) mod leg;
pub(crate) mod log;
pub(crate) mod market;
pub(crate) mod market_candle_1h;
pub(crate) mod network;
pub(crate) mod network_asset;
pub(crate) mod network_index;
pub(crate) mod schema;
pub(crate) mod session;
pub(crate) mod setting;
pub(crate) mod task;
pub(crate) mod transaction;
pub(crate) mod transaction_index;
pub(crate) mod trigger;
pub(crate) mod trigger_run;
pub(crate) mod user;
pub(crate) mod user_2fa;
pub(crate) mod user_key;
pub(crate) mod wallet;
pub(crate) mod wallet_index;

pub(crate) use asset::Asset;
pub(crate) use block::Block;
pub(crate) use caller::Caller;
pub(crate) use endpoint::{Endpoint, Endpoints, RpcEndpoints};
pub(crate) use endpoint_failure::{EndpointFailure, EndpointFailures};
pub(crate) use endpoint_route::{Route, Routes};
pub(crate) use endpoint_stats::EndpointStats;
pub(crate) use id::{Id, IdError};
pub(crate) use json::Json;
pub(crate) use leg::Leg;
pub(crate) use log::{Log, LogFilter};
pub(crate) use network::{Index, Network, NetworkName};
pub(crate) use network_asset::NetworkAsset;
pub(crate) use network_index::NetworkIndex;
pub(crate) use task::Task;

pub(crate) use session::{Client, Session};
pub(crate) use trigger::Trigger;
pub(crate) use trigger_run::TriggerRun;
pub(crate) use user::User;
pub(crate) use user_2fa::User2fa;
pub(crate) use user_key::UserKey;
pub(crate) use wallet::Wallet;

use anyhow::Context as _;

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
