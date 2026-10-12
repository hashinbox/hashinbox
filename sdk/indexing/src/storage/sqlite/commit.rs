//! Canonical blocks, atomic commit and offline network rescan.
//! The checkpoint read and its conditional write share one BEGIN IMMEDIATE
//! transaction, excluding intervening writers. A checkpoint coordinate alone is
//! not a mutation version: rescan can reproduce the same position and hash.

use sqlx::SqliteConnection;

use super::{Repository, checkpoint, commit, conflict, found, models};
use crate::{
    BlockRef, BoxFuture, IndexError, IndexStore, IndexedBatch, Network, OutputKey, Progress,
    PurgedRows,
};

use models::{
    Block, Ledger, NetworkCheckpoint, Output, Transaction, TransactionAddress, WalletIndex,
};

impl IndexStore for Repository {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move {
            let network = Self::network(network)?;
            let mut connection = self.database.connection().await?;
            checkpoint(&mut connection, &network).await
        })
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move {
            let network = Self::network(batch.network())?;
            let mut write = self.database.write().await?;
            let unknown = Self::apply(&mut write, &network, &batch).await?;
            commit(write).await?;
            Ok(unknown)
        })
    }

    /// Removes every indexing-owned row of one network inside one
    /// transaction, counts each kind, and resets what every observation of it
    /// claimed: one delete of the network's blocks, whose cascades take every
    /// fact, total and the checkpoint, and one reset of the observations'
    /// progress.
    ///
    /// One transaction: a network left with history but no checkpoint is
    /// itself a terminal state, so a purge that stopped halfway would replace
    /// one unrecoverable network with another.
    ///
    /// The wallet and observation rows survive and their claims do not. The observations are
    /// application state and a purged network re-indexes from the earliest
    /// registered birthday, while every claim described rows this purge just
    /// removed, and `WatchStore::add` preserves progress on an exact repeat by
    /// contract, so nothing could reset them afterwards.
    fn purge<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        Box::pin(async move {
            let network = Self::network(network)?;
            let mut write = self.database.write().await?;
            let purged = Block::purge(&mut write, &network).await?;
            WalletIndex::purge(&mut write, &network).await?;
            commit(write).await?;
            Ok(purged)
        })
    }
}

impl Repository {
    /// Writes one batch's transactions, credits, output changes, ledger rows
    /// with their running totals and progress, and the line it leaves behind —
    /// or nothing at all.
    ///
    /// Each table is written many rows to a statement.
    ///
    /// The checkpoint is read here, inside the write transaction, rather than by
    /// the caller before it: that is the whole compare-and-set. Whether the line
    /// moves is [`IndexedBatch::checkpoint`]'s answer, not a second method and
    /// not a flag.
    async fn apply(
        transaction: &mut SqliteConnection,
        network: &Network,
        batch: &IndexedBatch,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let current = checkpoint(transaction, network).await?;
        if current.as_ref() != batch.expected_checkpoint() {
            return Err(conflict("checkpoint changed before commit"));
        }
        // Every block a row or claim names is stored first, because every
        // other table refers to it.
        let named = batch
            .blocks()
            .iter()
            .map(|canonical| canonical.block.reference())
            .chain(batch.progress().iter().map(|claim| &claim.indexed_through))
            .chain(batch.checkpoint())
            .map(Block::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Block::insert_missing(transaction, network, &named).await?;
        // Every row of each table is gathered for the whole batch first, so a
        // table is written many rows to a statement; transactions go before
        // the memberships and ledger rows that refer to them.
        // Rows only for what is watched, read inside this transaction: a
        // counterparty nobody watches gets no ledger row, membership or
        // transaction.
        let watched = WalletIndex::find_watched(transaction, network).await?;
        let entries = Transaction::from_batch(batch, &watched)?;
        Transaction::insert_missing(transaction, &entries).await?;
        let members = TransactionAddress::from_batch(batch, &watched)?;
        TransactionAddress::insert_many(transaction, &members).await?;
        let lost =
            Ledger::insert_missing(transaction, Ledger::from_batch(batch, &watched)?).await?;
        let mut unknown = Vec::new();
        // Block by block, creations before spends, in the order the chain built
        // them. An output created in one block of this batch and spent in a
        // later one therefore leaves a row marked spent rather than no row at
        // all — which is what stops a replay of the creating block from offering
        // it again, and is the whole reason the batch does not fold.
        for canonical in batch.blocks() {
            let block = Block::try_from(canonical.block.reference())?.id(network);
            Output::insert_created(transaction, &block, &canonical.outputs.created).await?;
            unknown.extend(Output::update_spent(transaction, &block, &canonical.outputs).await?);
        }
        for claim in batch.progress() {
            Self::advance(transaction, network, claim).await?;
        }
        // After the claims, so a watch whose own rows this batch rewrote keeps
        // the progress it just proved: a holding that lost rows it was
        // inspected through goes back below them, and inspects them again.
        for ((address, asset), position) in &lost {
            let address = models::native_part(address);
            let held = WalletIndex::get_by_holding(transaction, network, (&address, asset)).await;
            if let Some(held) = found(held, "failed to read a watch")? {
                held.rewind(transaction, *position).await?;
            }
        }
        if let Some(line) = batch
            .checkpoint()
            .filter(|line| Some(*line) != current.as_ref())
        {
            NetworkCheckpoint::upsert(transaction, network, &Block::try_from(line)?).await?;
        }
        Ok(unknown)
    }

    /// Records one claim, after comparing what it expected against what is
    /// held.
    ///
    /// It lives on the observation's own `wallet_index` row, so withdrawing an
    /// observation takes its claim with it and a claim can never be paired
    /// with the wrong observation. The write is a compare-and-set against the
    /// value the claim's author says it read, deliberately **not** a monotonic
    /// rule: a batch redone under a smaller budget legitimately settles less
    /// than the write it is redoing, so what this refuses is a claim whose
    /// author was reading a different value.
    ///
    /// Two refusals, both retryable, and both about the caller's belief rather
    /// than the store's state: a claim naming an observation this file does not
    /// hold, and a claim whose `expected` is not what the row says. One read
    /// answers both.
    async fn advance(
        transaction: &mut SqliteConnection,
        network: &Network,
        claim: &Progress,
    ) -> Result<(), IndexError> {
        let held =
            WalletIndex::get_in_network(transaction, network, &claim.watch_id, &claim.asset.asset)
                .await;
        let Some(held) = found(held, "failed to read recorded progress")? else {
            return Err(conflict(
                "indexed progress names an observation this store does not hold",
            ));
        };
        if held.progress()?.as_ref() != claim.expected.as_ref() {
            return Err(conflict("recorded progress changed before this commit"));
        }
        let through = Block::try_from(&claim.indexed_through)?;
        held.update_progress(transaction, &through).await
    }
}
