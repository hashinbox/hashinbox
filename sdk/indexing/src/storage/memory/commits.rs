//! The in-memory store's write half: one commit, and the offline rescan.
//!
//! Separate from the parent module because that is the tables and the reads
//! over them; this is the one operation an index performs on them. What it
//! demonstrates is that [`crate::IndexStore::commit`] needs nothing a database
//! has — no conditional batch, no key framing, no versions — only that one
//! batch's history rows, output changes and progress claims become visible
//! together or not at all.
//!
//! **One rule for every block in the batch**: insert-if-absent, verify-if-
//! present, terminal on a row that disagrees. Whether the network's confirmed
//! line moves is decided by [`crate::IndexedBatch::checkpoint`] and changes
//! nothing about how a row is written, which is what collapsed two write paths
//! into this one.

use std::collections::BTreeMap;

use crate::{
    Address, BlockCredit, BlockRef, BoxFuture, IndexError, IndexErrorKind, IndexStore,
    IndexedBatch, Network, OutputChanges, OutputKey, Progress, PurgedRows, Purging,
};

use super::{Repository, Tables, conflict};

/// Keeps the rows `keep` accepts and counts the rows it dropped.
///
/// Every table of this store leads with the network that owns the row, so the
/// caller says which one it means by destructuring the key it already knows —
/// the same predicate [`BTreeMap::retain`] takes, which is the whole of the
/// removal.
fn dropped<K: Ord, V>(table: &mut BTreeMap<K, V>, mut keep: impl FnMut(&K) -> bool) -> u64 {
    let before = table.len();
    table.retain(|key, _| keep(key));
    u64::try_from(before - table.len()).unwrap_or(u64::MAX)
}

impl IndexStore for Repository {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.read(|tables| Ok(tables.checkpoints.get(network).cloned())) })
    }

    /// One batch, one change: the history rows, the folded output changes, every
    /// progress claim and the checkpoint land together or not at all, so a claim
    /// can never outlive the rows behind it.
    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move { self.apply(|tables| self.write(&batch, tables)) })
    }
}

impl Purging for Repository {
    fn purge_network<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        Box::pin(async move {
            self.apply(|tables| {
                let checkpoint = u64::from(tables.checkpoints.remove(network).is_some());
                let history = dropped(&mut tables.history, |(bound, ..)| bound != network);
                let credits = dropped(&mut tables.credits, |(bound, ..)| bound != network);
                let outputs = dropped(&mut tables.outputs, |(bound, ..)| bound != network);
                // Re-indexing this network forward sees every spend again, in
                // order, so the recorded spends go with the rows they guard.
                tables.spent.retain(|(bound, ..)| bound != network);
                // Watch progress claims rows this purge just removed, so it
                // goes with them; the observations themselves are application
                // state and stay. Not counted: it is index state, not a row a
                // rescan re-creates.
                tables.disclaim(network);
                Ok(PurgedRows {
                    checkpoint,
                    history,
                    // Movements live inside their history entry here too, so
                    // they are removed with it and counted as history.
                    movements: 0,
                    outputs,
                    credits,
                })
            })
        })
    }
}

impl Repository {
    /// The whole batch against one set of tables, inside the caller's change.
    fn write(
        &self,
        batch: &IndexedBatch,
        tables: &mut Tables,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let network = batch.network();
        if tables.checkpoints.get(network).as_ref() != batch.expected_checkpoint().as_ref() {
            return Err(conflict("checkpoint changed before commit"));
        }
        tables
            .settlements
            .insert(network.clone(), batch.settlement());
        let mut unknown = Vec::new();
        // Block by block, creations before spends, in the order the chain built
        // them: an output created and then spent inside one batch leaves a
        // recorded spend rather than nothing at all, so replaying the block that
        // created it cannot offer it again.
        for canonical in batch.blocks() {
            self.fill_history(canonical.transactions(), tables)?;
            self.fill_credits(network, canonical.credits(), canonical.block(), tables)?;
            unknown.extend(self.fill_outputs(network, canonical.outputs(), tables)?);
        }
        for claim in batch.progress() {
            Self::advance(network, claim, tables)?;
        }
        if let Some(line) = batch.checkpoint() {
            drop(tables.checkpoints.insert(network.clone(), line.clone()));
        }
        Ok(unknown)
    }

    /// Records one claim, after comparing what it expected against what is held.
    ///
    /// The compare is the whole guard, and it is not a monotonic rule: a batch
    /// redone under a smaller budget legitimately settles less than the write it
    /// is redoing, so what is refused is a claim whose author was reading a
    /// different value — never one that went down (`docs/design/030 §8 (ex-032)`,
    /// [`Progress`]).
    fn advance(network: &Network, claim: &Progress, tables: &mut Tables) -> Result<(), IndexError> {
        if tables.identities.get(&claim.watch_id) != Some(network) {
            return Err(conflict(
                "indexed progress names an observation this store does not hold",
            ));
        }
        let key = (network.clone(), claim.watch_id.clone());
        if tables.progress.get(&key) != claim.expected.as_ref() {
            return Err(conflict("recorded progress changed before this commit"));
        }
        drop(tables.progress.insert(key, claim.indexed_through.clone()));
        Ok(())
    }

    /// Insert-if-absent, verify-if-present: a newly watched address routinely
    /// already has rows in its own past, from every time it was somebody else's
    /// counterparty, and a row that differs is corruption rather than a
    /// duplicate.
    fn fill_history(
        &self,
        transactions: &crate::Transactions<crate::CanonicalTransaction>,
        tables: &mut Tables,
    ) -> Result<(), IndexError> {
        for transaction in transactions {
            for address in transaction.addresses() {
                self.fill_row(&address, transaction, tables)?;
            }
        }
        Ok(())
    }

    /// One address's row for one transaction, under the rule above.
    fn fill_row(
        &self,
        address: &Address,
        transaction: &crate::CanonicalTransaction,
        tables: &mut Tables,
    ) -> Result<(), IndexError> {
        let key = Self::history_key(&transaction.transaction_id.network, address, transaction);
        match tables.history.get(&key) {
            Some(stored) if stored == transaction => Ok(()),
            Some(_) => Err(Self::corrupt(
                "a committed transaction disagrees with the stored one",
            )),
            None => {
                Self::accrue_transaction(tables, address, transaction)?;
                drop(tables.history.insert(key, transaction.clone()));
                Ok(())
            }
        }
    }

    /// The block's transactionless credits, under the same rule as its rows:
    /// one already present is verified, one that differs is corruption.
    fn fill_credits(
        &self,
        network: &Network,
        credits: &[BlockCredit],
        block: &BlockRef,
        tables: &mut Tables,
    ) -> Result<(), IndexError> {
        for credit in credits {
            let key = Self::credit_key(network, credit, block.height);
            match tables.credits.get(&key) {
                Some((position, stored)) if *position == block.position && stored == credit => {}
                Some(_) => {
                    return Err(Self::corrupt(
                        "a committed block credit disagrees with the stored one",
                    ));
                }
                None => {
                    Self::accrue_credit(tables, credit)?;
                    drop(tables.credits.insert(key, (block.position, credit.clone())));
                }
            }
        }
        Ok(())
    }

    /// The batch's folded output changes, with one cancellation the fold itself
    /// cannot make: an output some recorded spend already named is not written,
    /// because that spend was applied above where this batch reaches and no batch
    /// will ever carry it again. The record survives the cancellation, so a
    /// redone batch refuses the output exactly as the first run did.
    fn fill_outputs(
        &self,
        network: &Network,
        changes: &OutputChanges,
        tables: &mut Tables,
    ) -> Result<Vec<OutputKey>, IndexError> {
        for output in &changes.created {
            let key = Self::output_key(network, &output.key());
            if tables.outputs.get(&key).is_some_and(|held| held != output) {
                return Err(Self::corrupt(
                    "a committed output disagrees with the stored one",
                ));
            }
            // A creation whose spending is already known is filled in without
            // becoming spendable, which is the one thing discovering a creation
            // later must never do.
            if tables.spent.contains(&key) {
                continue;
            }
            drop(tables.outputs.insert(key, output.clone()));
        }
        let required = changes.spent.iter().map(|key| (key, true));
        let tracked = changes.tracked_spends.iter().map(|key| (key, false));
        let mut unknown = Vec::new();
        for (spent, required) in required.chain(tracked) {
            let key = Self::output_key(network, spent);
            let held = tables.outputs.remove(&key).is_some();
            if !required {
                continue;
            }
            // The spend outlives the block that reported it whether or not it
            // matched: a later batch may still discover the creation below the
            // checkpoint, and no batch will ever carry this spend again.
            tables.spent.insert(key);
            if !held {
                unknown.push(spent.clone());
            }
        }
        Ok(unknown)
    }

    /// A stored row that disagrees with the one being written: terminal, because
    /// retrying cannot repair it.
    pub(super) fn corrupt(message: &'static str) -> IndexError {
        IndexError::new(IndexErrorKind::Store, message, false)
    }
}
