//! One model per table of `schema/sdk.sql` (the `history_entry` view lends
//! its rows to [`Transaction`]), and the column rules and statement mechanics
//! they share.
//!
//! A model owns every statement that reads or writes its table: each is an
//! associated function whose first parameter is the connection or transaction
//! it runs on. `find_*` answers every matching row, possibly none; `get_*`
//! answers exactly one, and a missing row is sqlx's `RowNotFound`, which the
//! port that asked turns into its own outcome where absence is legitimate.
//!
//! A model holds its row as the file does — SQLite's signed integers, canonical
//! amount text, raw hash bytes — so comparing two of them is comparing two rows,
//! which is how a re-presented fact is verified. Conversions to and from the
//! SDK's types are `TryFrom`, because both directions can refuse: a coordinate
//! above SQLite's integer range on the way in, a negative or half-written value
//! on the way out. A refusal is a non-retryable `Store` error whose message
//! names the column's role and never quotes the stored value.
//!
//! Each model reads itself from a row by column *name* (`sqlx::FromRow`), so
//! a statement may select its columns in any order and a view may lend its
//! rows to the models of the tables it joins. Every block a row names is read
//! as the joined `block_*` columns into an embedded [`Block`]. A model's
//! refusal of a row it reads travels inside the decode error and reaches the
//! caller as raised.
//!
//! Every id column holds a canonical `<chain>:<network>:<native>…` string
//! ([`canonical_id`]), so a join is one column.

mod block;
mod checkpoint;
mod ledger;
mod output;
mod transaction;
mod transaction_address;
mod wallet_index;

use std::str::FromStr;

use sqlx::{
    FromRow, QueryBuilder, Row as _, Sqlite, SqliteConnection,
    query::Query,
    sqlite::{SqliteArguments, SqliteRow},
};

pub(super) use block::Block;
pub(super) use checkpoint::NetworkCheckpoint;
pub(super) use ledger::Ledger;
pub(super) use output::Output;
pub(super) use transaction::Transaction;
pub(super) use transaction_address::TransactionAddress;
pub(super) use wallet_index::{Birthdays, WalletIndex};

use super::{IndexError, failure, store};
use crate::{ChainName, Decimal, Network};

pub(super) use crate::storage::canonical_id;

/// The last part of a [`canonical_id`], unescaped: the native value an
/// address id names.
pub(super) fn native_part(id: &str) -> String {
    let mut part = String::new();
    let mut escaped = false;
    for character in id.chars() {
        match (escaped, character) {
            (false, '\\') => escaped = true,
            (false, ':') => part.clear(),
            _ => {
                escaped = false;
                part.push(character);
            }
        }
    }
    part
}

/// The canonical id of `address`: `<chain>:<network>:<address>`.
pub(super) fn address_id(network: &Network, address: &str) -> String {
    canonical_id(&[network.chain.as_str(), &network.name, address])
}

/// One statement with its parameters bound, ready to run.
pub(super) type Bound<'q> = Query<'q, Sqlite, SqliteArguments<'q>>;

/// The most rows one statement writes, or looks up by key.
///
/// Every round trip to a connection's worker thread costs microseconds, so a
/// commit writes its rows many to a statement. The widest table, `ledger`,
/// binds 12 values a row, which keeps 64 rows far below SQLite's limit on
/// bound parameters.
const WIDEST: usize = 64;

/// One many-row statement run once per chunk of `rows`: consecutive chunks of
/// 64, 32, 16, 8, 4, 2 and 1 rows, a whole number of the widest and then the
/// binary digits of what remains.
///
/// A statement's text depends on its row count and every text is a prepared
/// statement the connection caches, so the counts are drawn from these seven
/// rather than from whatever a block happens to hold. Every model's
/// `insert_*`, upsert and keyed lookup goes through this one shape.
pub(super) struct Chunks<'q, T> {
    rows: &'q [T],
    context: &'static str,
}

impl<'q, T> Chunks<'q, T> {
    /// `rows`, and what a failure says was being attempted.
    pub(super) const fn new(rows: &'q [T], context: &'static str) -> Self {
        Self { rows, context }
    }

    /// The chunks themselves, widest first.
    fn split(&self) -> impl Iterator<Item = &'q [T]> + use<'q, T> {
        let mut rest = self.rows;
        std::iter::from_fn(move || {
            let size = match rest.len() {
                0 => return None,
                length if length >= WIDEST => WIDEST,
                length => 1 << length.ilog2(),
            };
            let (chunk, tail) = rest.split_at(size);
            rest = tail;
            Some(chunk)
        })
    }

    /// Runs the statement `statement` builds for every chunk; the number of
    /// rows they changed together.
    pub(super) async fn execute(
        self,
        connection: &mut SqliteConnection,
        statement: impl Fn(&'q [T]) -> QueryBuilder<'q, Sqlite>,
    ) -> Result<u64, IndexError> {
        let mut changed = 0;
        for chunk in self.split() {
            changed += statement(chunk)
                .build()
                .execute(&mut *connection)
                .await
                .map_err(|error| failure(&error, self.context))?
                .rows_affected();
        }
        Ok(changed)
    }

    /// Runs the statement `statement` builds for every chunk; every row they
    /// selected, as one `M` each, in no particular order across chunks.
    pub(super) async fn fetch<M>(
        self,
        connection: &mut SqliteConnection,
        statement: impl Fn(&'q [T]) -> QueryBuilder<'q, Sqlite>,
    ) -> Result<Vec<M>, IndexError>
    where
        M: for<'r> FromRow<'r, SqliteRow> + Send + Unpin,
    {
        let mut selected = Vec::new();
        for chunk in self.split() {
            selected.extend(
                statement(chunk)
                    .build_query_as::<M>()
                    .fetch_all(&mut *connection)
                    .await
                    .map_err(|error| failure(&error, self.context))?,
            );
        }
        Ok(selected)
    }
}

/// `statement` with `network`'s `(chain, network)` bound as its first two
/// parameters.
fn framed<'q>(statement: &'q str, network: &'q Network) -> Bound<'q> {
    sqlx::query(statement)
        .bind(network.chain.as_str())
        .bind(network.name.as_str())
}

/// Runs `query`, whose rows are bounded by its own `LIMIT` or key range,
/// and reads every row it selects as one `M`.
///
/// Fetched whole rather than streamed: a stream receives the rows from the
/// connection's worker thread one wake-up at a time, which costs a bounded
/// page about twice what one fetch does.
async fn fetched<M>(
    connection: &mut SqliteConnection,
    query: Bound<'_>,
    context: &str,
) -> Result<Vec<M>, IndexError>
where
    M: for<'r> FromRow<'r, SqliteRow>,
{
    query
        .fetch_all(connection)
        .await
        .map_err(|error| failure(&error, context))?
        .iter()
        .map(|row| M::from_row(row).map_err(|error| failure(&error, context)))
        .collect()
}

/// One named column of a row.
fn column<'r, T>(row: &'r SqliteRow, name: &str) -> Result<T, sqlx::Error>
where
    T: sqlx::Decode<'r, Sqlite> + sqlx::Type<Sqlite>,
{
    row.try_get(name)
}

/// A model's refusal of the row it is reading, carried through the decode
/// error so that it reaches the caller as raised (`super::failure`).
fn raised(error: IndexError) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(error))
}

/// [`raised`], for a refusal this module words.
fn refused(message: &str) -> sqlx::Error {
    raised(store(message))
}

/// The network a row's `chain` and `network` columns name.
fn network(row: &SqliteRow) -> Result<Network, sqlx::Error> {
    Ok(Network::new(
        ChainName::new(column::<String>(row, "chain")?),
        column::<String>(row, "network")?,
    ))
}

/// One block coordinate, narrowed to what SQLite can hold.
pub(super) fn stored(value: u64) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| store("a block coordinate is above the storable range"))
}

/// One stored block coordinate, widened back.
fn restored(value: i64) -> Result<u64, IndexError> {
    u64::try_from(value).map_err(|_| store("a stored block coordinate is negative"))
}

/// One monetary amount as the file holds it: canonical base-10 text,
/// independent of the in-memory big-integer implementation.
fn amount(encoded: &str) -> Result<Decimal, IndexError> {
    let value =
        Decimal::from_str(encoded).map_err(|_| store("stored amount is not a valid decimal"))?;
    if value.to_string() != encoded {
        return Err(store("stored amount is not canonical"));
    }
    value
        .to_atomic(value.scale())
        .map_err(|_| store("stored monetary amount must not be negative"))?;
    Ok(value)
}
