//! A payment's rows as its broadcast job writes them, over the
//! `transaction` and `transaction_index` models: [`record`], [`replace`] (a
//! bump) and [`restore`] (its undoing), and [`forget`], the one way a
//! pending transaction goes.

use base::TransactionIndex;
use diesel::SqliteConnection;

use crate::models::{
    transaction::{self, Transaction, TransactionId},
    transaction_index::{self, Why},
};

/// Records `pending`, not yet sent, watched by `index`, in the caller's
/// write, `broadcasted_at` now (before its broadcast): its outgoing legs
/// hold the sender's funds and coins from now on.
///
/// # Errors
///
/// [`transaction::Refused`] when a sender's index is not kept, or cannot
/// spend what it takes beyond what is in flight (the caller's write must
/// roll back); when a write fails.
pub(crate) fn record(
    connection: &mut SqliteConnection,
    pending: &mut Transaction,
    index: &TransactionIndex,
) -> anyhow::Result<()> {
    transaction::db::save_pending(connection, pending, i64::try_from(tasks::now())?)?;
    transaction_index::db::insert(connection, index)
}

/// Replaces pending `current` with `next`, watched by `index`, in the
/// caller's write: `current` forgotten (`replaced`) while its index watches
/// on, as either may land, then `next` recorded ([`record`]): the funds are
/// held once.
///
/// # Errors
///
/// As [`record`].
pub(crate) fn replace(
    connection: &mut SqliteConnection,
    current: &TransactionId,
    next: &mut Transaction,
    index: &TransactionIndex,
) -> anyhow::Result<()> {
    forget(connection, &[current], Why::Replaced)?;
    record(connection, next, index)
}

/// Undoes [`replace`] in the caller's write, `next` refused by every node:
/// `next` forgotten (`refused`) and its index deleted; `current`, as it was
/// kept before, pending again.
///
/// # Errors
///
/// As [`record`].
pub(crate) fn restore(
    connection: &mut SqliteConnection,
    current: &mut Transaction,
    next: &TransactionId,
) -> anyhow::Result<()> {
    forget(connection, &[next], Why::Refused)?;
    transaction_index::db::delete(connection, next)?;
    // Kept before: its `broadcasted_at` stays.
    transaction::db::save_pending(connection, current, i64::try_from(tasks::now())?)
}

/// Forgets the pending transactions `ids` for `why`, a mined one kept: each
/// is deleted with its legs, freeing what it held; its transaction index is
/// given up (`no_data`, `why`), so its broadcast job learns, unless it was
/// replaced: that one may still land. How many.
///
/// # Errors
///
/// When a write fails.
pub(crate) fn forget(
    connection: &mut SqliteConnection,
    ids: &[impl AsRef<str>],
    why: Why,
) -> anyhow::Result<usize> {
    let gone = transaction::db::delete_pending(connection, ids)?;
    if why != Why::Replaced {
        transaction_index::db::give_up(connection, &gone, why.as_str())?;
    }
    Ok(gone.len())
}
