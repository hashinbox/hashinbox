//! The durable watch list: what to observe, and how far it has been observed.
//!
//! **One row per observation**, in `wallet_index`: a wallet's address in one
//! asset, its address, birthday and progress — never the `wallet`
//! table, whose rows the wallets side files first (the foreign key is the
//! database's rule). [`WatchStore::all`]'s "one consistent snapshot" is a
//! single statement, so a watch cannot be handed another watch's progress.
//! Withdrawing a watch deletes its row; every indexed fact stays.
//!
//! `add` is one `BEGIN IMMEDIATE` transaction for the whole batch, so a wallet
//! registered for its native asset and a token either gets both rows or
//! neither. A *repeat* has to be recognised rather than refused, so the rows a
//! batch touches are read inside that transaction and compared.

use sqlx::SqliteConnection;

use super::{Repository, commit, found, models};
use crate::{BoxFuture, IndexError, IndexErrorKind, Network, Watch, WatchProgress, WatchStore};

use models::{Ledger, Output, Transaction, TransactionAddress, WalletIndex};

impl WatchStore for Repository {
    /// One transaction for the batch: every watch is read, compared and
    /// inserted inside it, so a conflicting definition anywhere in the batch
    /// leaves no row behind.
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let Some(network) = Self::one_network(&watches)? else {
                return Ok(());
            };
            let mut write = self.database.write().await?;
            for watch in &watches {
                Self::register(&mut write, &network, watch).await?;
            }
            commit(write).await
        })
    }

    /// The watch list and its progress from one snapshot: one statement.
    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        Box::pin(async move {
            let network = Self::network(network)?;
            let mut connection = self.database.connection().await?;
            WalletIndex::find_active(&mut connection, &network)
                .await?
                .into_iter()
                .map(WatchProgress::try_from)
                .collect()
        })
    }

    /// One transaction: the observation, its progress and its indexed facts
    /// go — its ledger rows and outputs, and, when no other asset of the
    /// address is still watched, its memberships, its spend-first tombstones
    /// and every transaction no membership names any more. Other watches'
    /// facts stay.
    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            if !watch.address.belongs_to(network) {
                return Err(IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "watch belongs to another network",
                    false,
                ));
            }
            let network = Self::network(network)?;
            let mut write = self.database.write().await?;
            Self::unregister(&mut write, &network, watch).await?;
            commit(write).await
        })
    }
}

impl Repository {
    /// The one network a batch is about, or `None` for an empty batch.
    ///
    /// A batch is atomic, and this store writes one network's rows per
    /// transaction, so a batch spanning two networks is refused rather than
    /// split into two transactions that could half-succeed.
    fn one_network(watches: &[Watch]) -> Result<Option<Network>, IndexError> {
        let Some(first) = watches.first() else {
            return Ok(None);
        };
        let network = first.address.network.clone();
        for watch in watches {
            watch.validate()?;
            if watch.address.network != network {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "a batch of watches must all belong to one network",
                    false,
                ));
            }
        }
        Self::network(&network).map(Some)
    }

    /// Deletes `watch`'s observation and the facts indexed for it.
    async fn unregister(
        transaction: &mut SqliteConnection,
        network: &Network,
        watch: &Watch,
    ) -> Result<(), IndexError> {
        let asset = watch.asset.asset.as_str();
        let held = WalletIndex::get_in_network(transaction, network, &watch.id, asset).await;
        if found(held, "failed to read an observation")?.is_none() {
            return Ok(());
        }
        WalletIndex::delete(transaction, network, &watch.id, asset).await?;
        let holding = models::address_id(network, &watch.address.value);
        Ledger::delete_from(transaction, (&holding, asset), i64::MIN).await?;
        let others =
            WalletIndex::find_active_by_address(transaction, network, &watch.address.value).await?;
        let last = others.is_empty();
        Output::delete_by_holding(transaction, (&holding, asset), last).await?;
        if last {
            let named = TransactionAddress::delete_by_address(transaction, &holding).await?;
            Transaction::delete_unreferenced(transaction, &named).await?;
        }
        Ok(())
    }

    /// Insert-if-absent, verify-if-present, for one watch: an observation
    /// already held is accepted only as an exact repeat; a new one deletes its
    /// holding's ledger rows from its birthday on, which its own inspection
    /// writes again in order (history is forward-only), and the database
    /// refuses it when no wallet is filed under its id or its
    /// `(address, asset)` is another id's.
    async fn register(
        transaction: &mut SqliteConnection,
        network: &Network,
        watch: &Watch,
    ) -> Result<(), IndexError> {
        let observed = WalletIndex::get(transaction, &watch.id, &watch.asset.asset).await;
        if let Some(observed) = found(observed, "failed to read an observation")? {
            return if WatchProgress::try_from(observed)?.watch == *watch {
                Ok(())
            } else {
                Err(IndexError::conflict("address already registered"))
            };
        }
        let birthday = i64::try_from(watch.start.0).unwrap_or(i64::MAX);
        let holding = models::address_id(network, &watch.address.value);
        Ledger::delete_from(transaction, (&holding, &watch.asset.asset), birthday).await?;
        WalletIndex::insert(transaction, watch).await
    }
}
