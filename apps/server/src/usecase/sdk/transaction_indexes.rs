//! `TransactionIndexStore` on the `transaction_index` model. A settle keeps
//! its blocks on the `block` model; giving one up forgets its transaction
//! (the `transaction` and `transaction_forgotten` models); the sends in
//! flight are the `transaction` model's pending rows. And a payment's rows
//! as its broadcast job writes them: [`record`], [`replace`] (a bump) and
//! [`restore`] (its undoing), and [`forget`], the one way a pending
//! transaction goes.

use base::{
    Address, BlockHeader, Future, IndexState, IntoIndex, Network, Networked as _, TransactionIndex,
    TransactionIndexStore,
};

use super::{Stores, blocks};
use diesel::SqliteConnection;

use crate::models::{
    transaction::{self, Transaction, TransactionId},
    transaction_forgotten::{self, Forgotten, Why},
    transaction_index,
};

/// Index `index`'s id: its transaction's.
fn id(index: &TransactionIndex) -> anyhow::Result<TransactionId> {
    Ok(TransactionId::new(index.network(), &index.transaction.id)?)
}

impl TransactionIndexStore for Stores {
    /// The index first (the chain's reads), then its write.
    fn insert<'a>(
        &'a self,
        index: &'a dyn IntoIndex<TransactionIndex>,
    ) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let index = index.index().await?;
            self.db
                .write(move |connection| transaction_index::db::insert(connection, &index))
                .await
        })
    }

    fn find_all<'a>(
        &'a self,
        network: &'a Network,
    ) -> Future<'a, anyhow::Result<Vec<TransactionIndex>>> {
        let network = network.clone();
        Box::pin(
            self.db
                .read(move |connection| transaction_index::db::find_all(connection, &network)),
        )
    }

    fn move_start<'a>(
        &'a self,
        index: &'a TransactionIndex,
        start: u64,
    ) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let id = id(index)?;
            self.db
                .write(move |connection| transaction_index::db::move_start(connection, &id, start))
                .await
        })
    }

    fn set_state<'a>(
        &'a self,
        index: &'a TransactionIndex,
        state: IndexState,
        reason: Option<&'a str>,
    ) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let (id, reason) = (id(index)?, reason.map(str::to_owned));
            self.db
                .write(move |connection| {
                    transaction_index::db::set_state(connection, &id, state, reason.as_deref())?;
                    Ok(())
                })
                .await
        })
    }

    /// One write: the final block and the one it is found in kept unless
    /// they are (another live block at either: `false`), the index on the
    /// final one.
    fn settle<'a>(
        &'a self,
        index: &'a TransactionIndex,
        found: Option<&'a BlockHeader>,
        header: &'a BlockHeader,
    ) -> Future<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let id = id(index)?;
            let (found, header) = (found.cloned(), header.clone());
            self.db
                .write(move |connection| {
                    let network = id.network();
                    let Some(block) = blocks::keep_bare(connection, network, &header)? else {
                        return Ok(false);
                    };
                    let found = match &found {
                        Some(found) => match blocks::keep_bare(connection, network, found)? {
                            Some(found) => Some(found),
                            None => return Ok(false),
                        },
                        None => None,
                    };
                    transaction_index::db::settle(connection, &id, &block, found.as_ref())?;
                    Ok(true)
                })
                .await
        })
    }

    /// One write: `no_data`, and its transaction forgotten while pending
    /// (`jump`).
    fn give_up<'a>(&'a self, index: &'a TransactionIndex) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let id = id(index)?;
            self.db
                .write(move |connection| {
                    // Given up whether or not its transaction is pending.
                    transaction_index::db::give_up(connection, &[&id], Why::Jump.as_str())?;
                    forget(connection, &[&id], Why::Jump)?;
                    Ok(())
                })
                .await
        })
    }

    fn delete<'a>(&'a self, index: &'a TransactionIndex) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let id = id(index)?;
            self.db
                .write(move |connection| {
                    transaction_index::db::delete(connection, &id)?;
                    Ok(())
                })
                .await
        })
    }

    fn find_sent<'a>(
        &'a self,
        sender: &'a Address,
    ) -> Future<'a, anyhow::Result<Vec<base::Transaction>>> {
        let sender = sender.clone();
        Box::pin(self.db.read(move |connection| {
            transaction::db::find_sent_by(connection, &sender)?
                .into_iter()
                .map(Transaction::transaction)
                .collect()
        }))
    }
}

/// Records `pending`, not yet sent, watched by `index`, in the caller's
/// write: its outgoing legs hold the sender's funds and coins from now on.
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
    transaction::db::save_pending(connection, pending)?;
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
/// kept before, pending again and no longer forgotten.
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
    transaction_forgotten::db::delete(connection, &current.id)?;
    transaction::db::save_pending(connection, current)
}

/// Forgets the pending transactions `ids` for `why`, a mined one kept: each
/// is deleted with its legs, freeing what it held, and kept in `transaction_forgotten`
/// as it was; its transaction index is given up (`no_data`, `why`), so its
/// broadcast job learns, unless it was replaced: that one may still land.
/// How many.
///
/// # Errors
///
/// When a write fails or a leg does not encode.
pub(crate) fn forget(
    connection: &mut SqliteConnection,
    ids: &[impl AsRef<str>],
    why: Why,
) -> anyhow::Result<usize> {
    let at = i64::try_from(tasks::now())?;
    let deleted = transaction::db::delete_pending(connection, ids)?;
    let gone: Vec<String> = deleted.iter().map(|row| row.id.clone().into()).collect();
    if why != Why::Replaced {
        transaction_index::db::give_up(connection, &gone, why.as_str())?;
    }
    let rows = deleted
        .into_iter()
        .map(|deleted| Forgotten::new(deleted, why, at))
        .collect::<anyhow::Result<Vec<_>>>()?;
    transaction_forgotten::db::insert(connection, &rows)?;
    Ok(rows.len())
}
