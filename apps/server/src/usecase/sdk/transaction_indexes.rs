//! `TransactionIndexStore` on the `transaction_index` model. Giving one up
//! forgets its transaction ([`indexes::forget`]); the sends in flight
//! are the `transaction` model's pending rows.

use base::{
    Address, Confirmed, Future, IndexState, IntoIndex, Network, Networked as _, TransactionIndex,
    TransactionIndexStore,
};

use super::Stores;

use crate::models::{
    transaction::{self, Transaction, TransactionId},
    transaction_index::{self, Why},
};
use crate::usecase::indexes::transaction as indexes;

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

    /// One write, no block kept.
    fn settle<'a>(
        &'a self,
        index: &'a TransactionIndex,
        confirmed: &'a Confirmed,
    ) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let (id, confirmed) = (id(index)?, *confirmed);
            self.db
                .write(move |connection| {
                    // Found: its outcome on its transaction row, as a block
                    // read would leave it.
                    if let Some(found) = confirmed.found {
                        transaction::db::set_mined(connection, &id, found, confirmed.reverted)?;
                    }
                    transaction_index::db::settle(connection, &id, &confirmed)
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
                    indexes::forget(connection, &[&id], Why::Jump)?;
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
