//! [`Notifying`]: an [`IndexStore`] that announces every commit it accepted.

use std::sync::Mutex;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::{
    BlockRef, BoxFuture, IndexError, IndexStore, IndexedBatch, Network, OutputKey, PurgedRows,
};

/// One watch advanced by one accepted commit: the block its inspection now
/// reaches.
///
/// A notification, not a fact to store: re-read the repository for the
/// answer. Nothing is replayed to a late subscriber; a consumer that must miss
/// nothing re-reads [`crate::WatchStore::all`] at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The network the commit belongs to.
    pub network: Network,
    /// The identity of the watch the commit advanced.
    pub watch: String,
    /// How far that watch is now inspected.
    pub block: BlockRef,
}

/// An [`IndexStore`] that delegates every call and, after a commit returned
/// `Ok`, sends one [`Change`] per progress claim of the batch to every
/// subscriber.
///
/// Wraps the index port only: whoever holds the wrapped store's other ports
/// keeps them. It observes and never vetoes; a subscriber that dropped its
/// stream is forgotten, and a slow one buffers.
pub struct Notifying<R> {
    inner: R,
    subscribers: Mutex<Vec<UnboundedSender<Change>>>,
}

impl<R: IndexStore> Notifying<R> {
    /// Wraps `inner`, with no subscribers yet.
    pub const fn new(inner: R) -> Self {
        Self {
            inner,
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// A stream of every [`Change`] from commits accepted after this call.
    pub fn changes(&self) -> UnboundedReceiver<Change> {
        let (sender, receiver) = unbounded();
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(sender);
        receiver
    }

    fn emit(&self, changes: &[Change]) {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.retain(|subscriber| {
            changes
                .iter()
                .all(|change| subscriber.unbounded_send(change.clone()).is_ok())
        });
    }
}

impl<R: IndexStore> IndexStore for Notifying<R> {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.inner.checkpoint(network)
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        let changes: Vec<Change> = batch
            .progress()
            .iter()
            .map(|claim| Change {
                network: batch.network().clone(),
                watch: claim.watch_id.clone(),
                block: claim.indexed_through.clone(),
            })
            .collect();
        Box::pin(async move {
            let unmatched = self.inner.commit(batch).await?;
            self.emit(&changes);
            Ok(unmatched)
        })
    }

    fn purge<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        self.inner.purge(network)
    }
}
