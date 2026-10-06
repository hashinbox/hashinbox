//! How much of the history behind a read the index can prove it holds.

use serde::{Deserialize, Serialize};

// Deliberately per read rather than per wallet: `overdrawn` is a fact about
// one fold and `incomplete` a claim about one start, and a single
// wallet-level state would have to pick one grain for both.
/// How much of the history behind an answer the index can prove it holds.
///
/// It is a property of the read, not of the wallet, and the balance and the
/// history page share the vocabulary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Completeness {
    /// Every change the address ever made is indexed, because observation
    /// started at or before its first use.
    ///
    /// The repository claims it only for an address observed from the chain's
    /// first block and inspected up to the line the answer was read at. Treat
    /// its absence as "not proven", never as "wrong".
    CompleteHistory,
    /// Every change since observation began is indexed, and nothing proves it
    /// began early enough. The ordinary answer.
    Incomplete,
    /// The fold went negative, which a complete index cannot do: more left the
    /// address than ever reached it, so credits are provably missing and
    /// `amount` is zero because no honest number exists.
    Overdrawn,
}

impl From<indexing::Completeness> for Completeness {
    fn from(value: indexing::Completeness) -> Self {
        match value {
            indexing::Completeness::CompleteHistory => Self::CompleteHistory,
            indexing::Completeness::Incomplete => Self::Incomplete,
            indexing::Completeness::Overdrawn => Self::Overdrawn,
        }
    }
}
