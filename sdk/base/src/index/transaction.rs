//! A sent transaction's index ([`TransactionIndex`]): it starts at the tip
//! it was broadcast at, moves forward with every block kept, and notes the
//! block the transaction is found in. What a sender asks to watch
//! ([`TransactionIndexTarget`]), where indexes are kept
//! ([`TransactionIndexStore`]), and the part of the indexer that closes
//! their gaps ([`TransactionIndexer`]), with the chain's [`Confirmer`].

use std::sync::Arc;

use anyhow::{Result, ensure};

use super::{CLOSES, IndexState, Indexed, IntoIndex, Progress, Skipped, behind, lowest_next};
use crate::{
    Address, BlockHeader, Chained, Future, Indexes, Network, Networked, NotServed, Tips,
    Transaction, TransactionId,
};

/// How far the blocks after a transaction's broadcast have been read for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionIndex {
    /// The transaction looked for.
    pub transaction: TransactionId,
    /// The tip when it was broadcast: the first block it reads.
    pub broadcasted_at: u64,
    /// Solana's last valid block height; `None`: it never expires.
    pub expires_height: Option<u64>,
    /// The block it is mined in.
    pub found: Option<BlockHeader>,
    /// How far it has read.
    pub progress: Progress,
    /// Its network's final block, as the indexer last read it: filled in by
    /// a store when it reads the index; `None` while unknown.
    pub confirmed: Option<u64>,
}

/// Where a watched transaction stands ([`TransactionIndex::status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    /// In a final block: paid.
    Final,
    /// Can never land: read past its `expires_height` (Solana), not found.
    Dead,
    /// Given up: its gap could not be closed (a Bitcoin jump).
    GivenUp,
    /// In none of the blocks read since it was broadcast, through the final
    /// one.
    Missing,
    /// Not known yet: found above the final block, or not read that far.
    Unknown,
}

impl TransactionIndex {
    /// Where its transaction stands, by what it has read and its network's
    /// final block.
    #[must_use]
    pub fn status(&self) -> TransactionStatus {
        let is_final = |position: u64| self.confirmed.is_some_and(|at| position <= at);
        if let Some(found) = &self.found {
            if is_final(found.position) {
                return TransactionStatus::Final;
            }
            return TransactionStatus::Unknown;
        }
        if self.progress.state == IndexState::NoData {
            return TransactionStatus::GivenUp;
        }
        let Some(through) = &self.progress.indexed_at else {
            return TransactionStatus::Unknown;
        };
        if self
            .expires_height
            .is_some_and(|expires| through.height > expires)
        {
            return TransactionStatus::Dead;
        }
        let read = self.progress.state == IndexState::Indexing
            && self.confirmed.is_some_and(|at| through.position >= at);
        if read {
            return TransactionStatus::Missing;
        }
        TransactionStatus::Unknown
    }

    /// Whether its transaction is stuck and worth a bump, judged by the
    /// blocks it read, so a lagging indexer never makes one due:
    ///
    /// - one that expires (Solana) cannot be replaced in flight: due once
    ///   it can never land (`Dead`);
    /// - one that never expires is replaced in flight: due once it is in
    ///   none of the blocks read through the final one, and those reach
    ///   `bump_after` past its `broadcasted_at`.
    ///
    /// How many bumps a payment may have is its sender's to judge.
    #[must_use]
    pub fn bump_due(&self, bump_after: u64) -> bool {
        let Some(read) = &self.progress.indexed_at else {
            return false;
        };
        match self.status() {
            TransactionStatus::Dead => true,
            TransactionStatus::Missing => {
                self.expires_height.is_none()
                    && read.position.saturating_sub(self.broadcasted_at) >= bump_after
            }
            _ => false,
        }
    }
}

impl Indexed for TransactionIndex {
    fn start(&self) -> u64 {
        self.broadcasted_at
    }

    fn progress(&self) -> &Progress {
        &self.progress
    }

    /// As for any index, but none once found: a transaction is mined once,
    /// so it stays on the block it was found in, and no block after is read
    /// for it. A reorg that takes that block back clears `found` and it
    /// reads on.
    fn next(&self) -> Option<u64> {
        if self.found.is_some() {
            return None;
        }
        self.progress
            .indexed_at
            .as_ref()
            .map_or(Some(self.broadcasted_at), |header| {
                header.position.checked_add(1)
            })
    }
}

/// A finished index is its own target.
impl IntoIndex<Self> for TransactionIndex {
    fn index(&self) -> Future<'_, Result<Self>> {
        let index = self.clone();
        Box::pin(async move { Ok(index) })
    }
}

impl Networked for TransactionIndex {
    fn network(&self) -> &Network {
        &self.transaction.network
    }
}

/// A transaction about to be broadcast, to be looked for in the blocks
/// after; the chain fills in the rest ([`IntoIndex`]).
pub struct TransactionIndexTarget {
    chain: Arc<dyn Chained>,
    transaction: TransactionId,
    expires_height: Option<u64>,
}

impl TransactionIndexTarget {
    /// `transaction`, on `chain`; it never expires.
    #[must_use]
    pub fn new(chain: Arc<dyn Chained>, transaction: TransactionId) -> Self {
        Self {
            chain,
            transaction,
            expires_height: None,
        }
    }

    /// Solana: its blockhash's last valid block height.
    #[must_use]
    pub const fn with_expires_height(mut self, height: u64) -> Self {
        self.expires_height = Some(height);
        self
    }
}

/// `broadcasted_at` is the produced tip now: it is made just before the
/// broadcast.
impl IntoIndex<TransactionIndex> for TransactionIndexTarget {
    fn index(&self) -> Future<'_, Result<TransactionIndex>> {
        Box::pin(async move {
            let network = self.chain.network();
            ensure!(
                self.transaction.network == *network,
                "{:?} is not of {network:?}",
                self.transaction
            );
            Ok(TransactionIndex {
                transaction: self.transaction.clone(),
                broadcasted_at: self.chain.tips().await?.produced,
                expires_height: self.expires_height,
                found: None,
                progress: Progress::default(),
                confirmed: None,
            })
        })
    }
}

/// What a [`Confirmer`] judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Confirmed {
    /// The final block it was judged at.
    pub position: u64,
    /// The block it is mined in, at or below `position`; `None`: not mined
    /// through it.
    pub found: Option<u64>,
}

/// A chain that can tell whether a transaction was mined, by its id:
/// optional, a [`Service`](crate::Service) may lack it.
pub trait Confirmer: Networked + Send + Sync {
    /// Whether transaction `id` was mined, judged at a final block at or
    /// after `at`. `None`: the chain has not finalized `at` yet.
    ///
    /// # Errors
    ///
    /// When the node cannot answer.
    fn confirm<'a>(
        &'a self,
        id: &'a TransactionId,
        at: u64,
    ) -> Future<'a, Result<Option<Confirmed>>>;
}

/// Where transaction indexes are kept.
pub trait TransactionIndexStore: Send + Sync {
    /// Keeps the index `index` becomes; one already kept stays as it is.
    ///
    /// # Errors
    ///
    /// When the chain cannot answer, or the store cannot write.
    fn insert<'a>(&'a self, index: &'a dyn IntoIndex<TransactionIndex>) -> Future<'a, Result<()>>;

    /// Every index of `network`, whatever its state.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_all<'a>(&'a self, network: &'a Network) -> Future<'a, Result<Vec<TransactionIndex>>>;

    /// The lowest [`Indexed::next`] of `network`'s [`IndexState::Indexing`]
    /// indexes at or above `from` and at or below `through`; with `None`,
    /// at or above `from`. By default from [`Self::find_all`].
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_next<'a>(
        &'a self,
        network: &'a Network,
        from: u64,
        through: Option<u64>,
    ) -> Future<'a, Result<Option<u64>>> {
        Box::pin(async move { Ok(lowest_next(&self.find_all(network).await?, from, through)) })
    }

    /// `network`'s [`IndexState::Indexing`] indexes whose [`Indexed::next`]
    /// is below `below`, furthest behind first, at most `limit`. By default
    /// from [`Self::find_all`].
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_behind<'a>(
        &'a self,
        network: &'a Network,
        below: u64,
        limit: usize,
    ) -> Future<'a, Result<Vec<TransactionIndex>>> {
        Box::pin(async move { Ok(behind(self.find_all(network).await?, below, limit)) })
    }

    /// Moves the `broadcasted_at` of `index` to `start`, only while it has
    /// not started (`indexed_at` unset).
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn move_start<'a>(&'a self, index: &'a TransactionIndex, start: u64) -> Future<'a, Result<()>>;

    /// Sets the state of `index`, and its reason (`None` clears it).
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn set_state<'a>(
        &'a self,
        index: &'a TransactionIndex,
        state: IndexState,
        reason: Option<&'a str>,
    ) -> Future<'a, Result<()>>;

    /// Settles `index` at a final block (its header `header`), in one
    /// write: that block kept unless it is, and the one it is `found` in
    /// (mined: final, as it is at or below it); the index continuing from
    /// the block after. `false`: another live block at one of them;
    /// nothing changed; try again later.
    ///
    /// # Errors
    ///
    /// When the store cannot read or write, or a block does not link onto
    /// the kept blocks around it.
    fn settle<'a>(
        &'a self,
        index: &'a TransactionIndex,
        found: Option<&'a BlockHeader>,
        header: &'a BlockHeader,
    ) -> Future<'a, Result<bool>>;

    /// Gives `index` up, in one write: [`IndexState::NoData`], and its
    /// transaction forgotten while pending (`jump`), as a wallet's jump
    /// forgets it: one mined inside the gap is never read.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn give_up<'a>(&'a self, index: &'a TransactionIndex) -> Future<'a, Result<()>>;

    /// Forgets `index`: its sender is done with it.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn delete<'a>(&'a self, index: &'a TransactionIndex) -> Future<'a, Result<()>>;

    /// The transactions in flight that `sender` pays (a spent, burned or
    /// fee leg), as signed: an Ethereum sender's next nonce follows them,
    /// whatever a node's mempool shows.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_sent<'a>(&'a self, sender: &'a Address) -> Future<'a, Result<Vec<Transaction>>>;
}

/// A [`TransactionIndexStore`] that keeps nothing: nothing watched, nothing
/// in flight. For a caller that tracks no sends (tests, examples).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTransactionIndexes;

impl TransactionIndexStore for NoTransactionIndexes {
    fn insert<'a>(&'a self, _: &'a dyn IntoIndex<TransactionIndex>) -> Future<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn find_all<'a>(&'a self, _: &'a Network) -> Future<'a, Result<Vec<TransactionIndex>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn move_start<'a>(&'a self, _: &'a TransactionIndex, _: u64) -> Future<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn set_state<'a>(
        &'a self,
        _: &'a TransactionIndex,
        _: IndexState,
        _: Option<&'a str>,
    ) -> Future<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn settle<'a>(
        &'a self,
        _: &'a TransactionIndex,
        _: Option<&'a BlockHeader>,
        _: &'a BlockHeader,
    ) -> Future<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn give_up<'a>(&'a self, _: &'a TransactionIndex) -> Future<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn delete<'a>(&'a self, _: &'a TransactionIndex) -> Future<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn find_sent<'a>(&'a self, _: &'a Address) -> Future<'a, Result<Vec<Transaction>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The transaction indexes of one network, as one part of its
/// [`Indexer`](crate::Indexer): their gaps below the line closed by the
/// chain ([`Confirmer`]: a settle) or, without one, given up.
pub struct TransactionIndexer {
    store: Arc<dyn TransactionIndexStore>,
    chain: Arc<dyn Chained>,
    confirmer: Option<Arc<dyn Confirmer>>,
}

impl TransactionIndexer {
    /// The indexes of `store` on `chain`, settled through `confirmer` when
    /// the chain has one.
    #[must_use]
    pub fn new(
        store: Arc<dyn TransactionIndexStore>,
        chain: Arc<dyn Chained>,
        confirmer: Option<Arc<dyn Confirmer>>,
    ) -> Self {
        Self {
            store,
            chain,
            confirmer,
        }
    }

    /// Closes `index`'s gap. With a confirmer, settles it at a final block
    /// at or above `at`: asks whether it was mined through that block and,
    /// once the chain answers and serves both blocks, writes it
    /// ([`TransactionIndexStore::settle`]). Not answerable yet: it stays as
    /// it is for the next step. Without (Bitcoin), gives it up
    /// ([`TransactionIndexStore::give_up`]).
    async fn close_one(&self, index: &TransactionIndex, at: u64) -> Result<()> {
        let Some(confirmer) = &self.confirmer else {
            return self.store.give_up(index).await;
        };
        let Some(confirmed) = confirmer.confirm(&index.transaction, at).await? else {
            return Ok(());
        };
        let Some(block) = self.chain.block(confirmed.position).await? else {
            return Ok(());
        };
        let found = match confirmed.found {
            Some(position) => match self.chain.block(position).await? {
                Some(found) => Some(found.header),
                None => return Ok(()),
            },
            None => None,
        };
        let settled = self.store.settle(index, found.as_ref(), &block.header);
        settled.await?;
        Ok(())
    }
}

impl Networked for TransactionIndexer {
    fn network(&self) -> &Network {
        self.chain.network()
    }
}

impl Indexes for TransactionIndexer {
    fn next<'a>(&'a self, from: u64, through: Option<u64>) -> Future<'a, Result<Option<u64>>> {
        self.store.find_next(self.network(), from, through)
    }

    /// Closes the gaps of at most a few indexes whose next block is below
    /// `line` ([`Self::close_one`]), with `tips.confirmed` the final block;
    /// one failing does not stop the others, and the first failure is the
    /// answer.
    fn close<'a>(&'a self, line: u64, tips: Tips) -> Future<'a, Result<()>> {
        Box::pin(async move {
            let behind = self.store.find_behind(self.network(), line, CLOSES);
            let mut failed = None;
            for index in &behind.await? {
                if let Err(error) = self.close_one(index, tips.confirmed).await {
                    failed.get_or_insert(error);
                }
            }
            failed.map_or(Ok(()), Err)
        })
    }

    fn skip<'a>(&'a self, below: u64, start: u64) -> Future<'a, Result<Skipped>> {
        Box::pin(async move {
            let mut skipped = Skipped::default();
            let behind = self.store.find_behind(self.network(), below, usize::MAX);
            for index in &behind.await? {
                let Some(position) = index.next() else {
                    continue;
                };
                if index.progress.indexed_at.is_none() {
                    self.store.move_start(index, start).await?;
                    skipped.moved = true;
                } else {
                    let reason = NotServed { position }.to_string();
                    let state = IndexState::NoData;
                    self.store.set_state(index, state, Some(&reason)).await?;
                    skipped.stopped = true;
                }
            }
            Ok(skipped)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{IndexState, Progress, TransactionIndex, TransactionStatus};
    use crate::{BlockHash, BlockHeader, Chain, Network, TransactionId};

    fn header(position: u64) -> BlockHeader {
        BlockHeader {
            position,
            height: position,
            hash: BlockHash(position.to_be_bytes().to_vec()),
            parent: None,
            timestamp: None,
        }
    }

    /// Broadcast at 10, final block 20.
    fn index(found: Option<u64>, through: Option<u64>, state: IndexState) -> TransactionIndex {
        TransactionIndex {
            transaction: TransactionId::new(Network::new(Chain::new("c"), "n"), "t"),
            broadcasted_at: 10,
            expires_height: None,
            found: found.map(header),
            progress: Progress {
                indexed_at: through.map(header),
                state,
                reason: None,
            },
            confirmed: Some(20),
        }
    }

    #[test]
    fn its_status_follows_what_it_read() {
        use IndexState::{Indexing, NoData, Paused};
        use TransactionStatus::{Dead, Final, GivenUp, Missing, Unknown};
        let cases = [
            (index(Some(20), Some(25), Indexing), Final),
            (index(Some(21), Some(25), Indexing), Unknown),
            (index(None, Some(25), NoData), GivenUp),
            (index(None, None, Indexing), Unknown),
            (index(None, Some(19), Indexing), Unknown),
            (index(None, Some(20), Indexing), Missing),
            (index(None, Some(25), Paused), Unknown),
        ];
        for (index, status) in cases {
            assert_eq!(index.status(), status, "{index:?}");
        }
        let expired = TransactionIndex {
            expires_height: Some(24),
            ..index(None, Some(25), Indexing)
        };
        assert_eq!(expired.status(), Dead);
        let unknown_final = TransactionIndex {
            confirmed: None,
            ..index(Some(20), Some(25), Indexing)
        };
        assert_eq!(unknown_final.status(), Unknown);
    }
}
