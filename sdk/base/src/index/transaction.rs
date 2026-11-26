//! A sent transaction's index ([`TransactionIndex`]): it starts at the tip
//! it was broadcast at, moves forward with every block kept, and notes the
//! block the transaction is found in. What a sender asks to watch
//! ([`TransactionIndexTarget`]), where indexes are kept
//! ([`TransactionIndexStore`]), and the part of the indexer that closes
//! their gaps ([`TransactionIndexer`]), with the chain's [`Confirmer`].

use std::sync::Arc;

use tracing::Instrument as _;

use anyhow::{Result, ensure};

use super::{CLOSES, IndexState, Indexed, IntoIndex, Progress, Skipped, behind, lowest_next};
use crate::{
    Address, Chained, Future, Indexes, Network, Networked, NotServed, Transaction, TransactionId,
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
    /// The position it is mined in; `None` until found. A found index is
    /// no longer read.
    pub found: Option<u64>,
    /// Whether its transaction, found, reverted (only its fee paid): its
    /// transaction's own outcome, filled in by a store when it reads the
    /// index.
    pub reverted: bool,
    /// How far it has read.
    pub progress: Progress,
    /// Its network's confirmed position, as the indexer last read the tip
    /// ([`crate::Tips::confirmed`]): filled in by a store when it reads the index;
    /// `None` while unknown.
    pub confirmed: Option<u64>,
}

/// Where a watched transaction stands ([`TransactionIndex::status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    /// In a final block: paid.
    Final,
    /// In a final block, reverted: only its fee paid.
    Reverted,
    /// Given up: its gap could not be closed (a Bitcoin jump).
    GivenUp,
    /// In none of the blocks read since it was broadcast, through the final
    /// one.
    Missing,
    /// Not known yet: found above the final block, or not read that far.
    Unknown,
}

impl TransactionIndex {
    /// Where its transaction stands, by its state and its network's
    /// confirmed position. Expiry (Solana) is its sender's to judge, by the
    /// height of a block at or below it ([`Self::expired`]).
    #[must_use]
    pub fn status(&self) -> TransactionStatus {
        let confirmed = self.confirmed;
        let read = (self.progress.indexed_at.zip(confirmed)).is_some_and(|(at, end)| at >= end);
        if let Some(found) = self.found {
            return match confirmed {
                Some(end) if found <= end && self.reverted => TransactionStatus::Reverted,
                Some(end) if found <= end => TransactionStatus::Final,
                _ => TransactionStatus::Unknown,
            };
        }
        match self.progress.state {
            IndexState::NoData => TransactionStatus::GivenUp,
            IndexState::Indexing if read => TransactionStatus::Missing,
            IndexState::Indexing | IndexState::Paused => TransactionStatus::Unknown,
        }
    }

    /// Whether its transaction can never land (Solana): [`TransactionStatus::Missing`]
    /// (read through the confirmed position, not found) and `height`, of a
    /// block at or below that position, above its `expires_height`.
    #[must_use]
    pub fn expired(&self, height: u64) -> bool {
        self.status() == TransactionStatus::Missing
            && self.expires_height.is_some_and(|expires| expires < height)
    }

    /// Whether its transaction is stuck and worth a bump, judged by the
    /// blocks it read, so a lagging indexer never makes one due:
    ///
    /// - one that expires (Solana) cannot be replaced in flight: never due;
    ///   its sender drops it once [`Self::expired`] and signs
    ///   anew;
    /// - one that never expires is replaced in flight: due once it is in
    ///   none of the blocks read through the final one, and those reach
    ///   `bump_after` past its `broadcasted_at`.
    ///
    /// How many bumps a payment may have is its sender's to judge.
    #[must_use]
    pub fn bump_due(&self, bump_after: u64) -> bool {
        let Some(read) = self.progress.indexed_at else {
            return false;
        };
        self.status() == TransactionStatus::Missing
            && self.expires_height.is_none()
            && read.saturating_sub(self.broadcasted_at) >= bump_after
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
            .map_or(Some(self.broadcasted_at), |position| {
                position.checked_add(1)
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
                broadcasted_at: self.chain.tips().await?.position,
                expires_height: self.expires_height,
                found: None,
                reverted: false,
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
    /// Whether it reverted in `found` (only its fee paid); `false` when not
    /// found.
    pub reverted: bool,
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

    /// Settles `index` as the chain's [`Confirmer`] judged it (`confirmed`),
    /// in one write, no block kept: read through `confirmed.position`, the
    /// index continuing from the position after; when mined, found at
    /// `confirmed.found`, its transaction's outcome `confirmed.reverted`.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn settle<'a>(
        &'a self,
        index: &'a TransactionIndex,
        confirmed: &'a Confirmed,
    ) -> Future<'a, Result<()>>;

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

    fn settle<'a>(&'a self, _: &'a TransactionIndex, _: &'a Confirmed) -> Future<'a, Result<()>> {
        Box::pin(async { Ok(()) })
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
    /// once the chain answers, writes it ([`TransactionIndexStore::settle`]).
    /// Not answerable yet: it stays as it is for the next step. Without
    /// (Bitcoin), gives it up ([`TransactionIndexStore::give_up`]).
    async fn close_one(&self, index: &TransactionIndex, at: u64) -> Result<()> {
        let Some(confirmer) = &self.confirmer else {
            return self.store.give_up(index).await;
        };
        let Some(confirmed) = confirmer.confirm(&index.transaction, at).await? else {
            return Ok(());
        };
        self.store.settle(index, &confirmed).await
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
    /// `line` (`close_one`), at `confirmed`, the confirmed position;
    /// one failing does not stop the others, and the first failure is the
    /// answer. Each runs in an `index.close` span (`txid`).
    fn close<'a>(&'a self, line: u64, confirmed: u64) -> Future<'a, Result<()>> {
        Box::pin(async move {
            let behind = self.store.find_behind(self.network(), line, CLOSES);
            let mut failed = None;
            for index in &behind.await? {
                let txid = &index.transaction.id;
                let span = tracing::info_span!("index.close", txid = %txid);
                if let Err(error) = self.close_one(index, confirmed).instrument(span).await {
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
    use crate::{Chain, Network, TransactionId};

    /// Broadcast at 10, confirmed position 20.
    fn index(found: Option<u64>, through: Option<u64>, state: IndexState) -> TransactionIndex {
        TransactionIndex {
            transaction: TransactionId::new(Network::new(Chain::new("c"), "n"), "t"),
            broadcasted_at: 10,
            expires_height: None,
            found,
            reverted: false,
            progress: Progress {
                indexed_at: through,
                state,
                reason: None,
            },
            confirmed: Some(20),
        }
    }

    #[test]
    fn its_status_follows_its_state() {
        use IndexState::{Indexing, NoData, Paused};
        use TransactionStatus as Status;
        let reverted = |found| TransactionIndex {
            reverted: true,
            ..index(Some(found), Some(25), Indexing)
        };
        let cases = [
            (index(Some(20), Some(25), Indexing), Status::Final),
            (index(Some(21), Some(25), Indexing), Status::Unknown),
            (reverted(20), Status::Reverted),
            (reverted(21), Status::Unknown),
            (index(None, Some(25), NoData), Status::GivenUp),
            (index(None, None, Indexing), Status::Unknown),
            (index(None, Some(19), Indexing), Status::Unknown),
            (index(None, Some(20), Indexing), Status::Missing),
            (index(None, Some(25), Paused), Status::Unknown),
        ];
        for (index, status) in cases {
            assert_eq!(index.status(), status, "{index:?}");
        }
        let found = TransactionIndex {
            expires_height: Some(19),
            ..index(Some(20), Some(20), Indexing)
        };
        assert_eq!(found.status(), Status::Final);
        let unknown_final = TransactionIndex {
            confirmed: None,
            ..index(Some(20), Some(25), Indexing)
        };
        assert_eq!(unknown_final.status(), Status::Unknown);
    }

    #[test]
    fn solana_expiry_is_by_the_height_of_a_block_at_or_below_the_confirmed_position() {
        // Read through the confirmed position 20, not found; a kept block at
        // or below it has `height`.
        let expiring = |expires, through| TransactionIndex {
            expires_height: Some(expires),
            ..index(None, Some(through), IndexState::Indexing)
        };
        assert!(expiring(14, 20).expired(15));
        assert!(!expiring(15, 20).expired(15), "not past yet");
        assert!(!expiring(14, 19).expired(15), "not read through");
        assert!(
            !index(None, Some(20), IndexState::Indexing).expired(15),
            "never expires"
        );
        let found = TransactionIndex {
            expires_height: Some(14),
            ..index(Some(20), Some(20), IndexState::Indexing)
        };
        assert!(!found.expired(15), "found");
        assert_eq!(expiring(14, 20).status(), TransactionStatus::Missing);
    }
}
