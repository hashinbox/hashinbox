//! `transaction_forgotten`: the pending transactions the server forgot (design 084), as
//! they were then, with why. Forgetting deletes a `transaction` row and its
//! legs; this keeps what it was.

use anyhow::Context as _;
use diesel::prelude::*;

use super::{schema::transaction_forgotten, transaction::Transaction};

/// Why a pending transaction was forgotten: its `reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// Every node refused it, or it was never sent after one that was.
    Refused,
    /// The chain proved it can never mine.
    Dead,
    /// Its index (a wallet's or its own) jumped past a gap, where it may
    /// have mined; or its own was given up, or found it mined inside one.
    Jump,
}

impl Why {
    /// As stored.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Dead => "dead",
            Self::Jump => "jump",
        }
    }
}

/// One `transaction_forgotten` row.
#[derive(Insertable)]
#[diesel(table_name = transaction_forgotten)]
pub(crate) struct Forgotten {
    id: String,
    chain: String,
    network: String,
    txid: String,
    reason: &'static str,
    /// Unix ms.
    forgotten_at: i64,
    bytes: Vec<u8>,
    /// Its legs as `transaction_with_legs` reads them, a JSON array.
    legs: String,
}

impl Forgotten {
    /// `transaction`, with its legs, forgotten for `why` at `at` (Unix ms).
    ///
    /// # Errors
    ///
    /// When its legs do not encode.
    pub(crate) fn new(transaction: Transaction, why: Why, at: i64) -> anyhow::Result<Self> {
        Ok(Self {
            id: transaction.id.as_ref().to_owned(),
            legs: serde_json::to_string(&transaction.legs).context("legs did not encode")?,
            chain: transaction.chain,
            network: transaction.network,
            txid: transaction.txid,
            reason: why.as_str(),
            forgotten_at: at,
            bytes: transaction.bytes,
        })
    }
}

/// The `transaction_forgotten` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Keeps `rows`; a txid kept before takes the new row.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        rows: &[Forgotten],
    ) -> QueryResult<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        diesel::replace_into(transaction_forgotten::table)
            .values(rows)
            .execute(connection)
    }
}
