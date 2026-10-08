//! Settling an unknown submission outcome in the background.
//!
//! The sender stays reserved until one of two proofs holds: the node or the
//! index shows the transaction (present), or its blockhash expired and the
//! index covers that height without it (absent). Until then the check
//! repeats with a doubling backoff, restarted whenever the index moves.

use std::{sync::Arc, time::Duration};

use base::{BlockRef, BoxFuture};
use indexing::{BlockCommit, Checkpoint, Listener, Network};
use tokio::sync::watch;

use super::{Presence, reservation::Reservations};
use crate::native::{SignedTransaction, rpc::Submitter};

/// Announces each committed block to this recovery, so a retained submission
/// rechecks as soon as the index moves instead of waiting out its backoff.
///
/// It lives here, with the only thing that reads it, rather than on the
/// indexer: an `indexing::Listener` any caller may register, and one an
/// indexing-only caller never allocates. It is *one entry* of the service's
/// listener list, so a caller's own listener is announced beside it rather
/// than displacing it.
pub(crate) struct CommitSignal(watch::Sender<Option<BlockRef>>);

impl CommitSignal {
    /// The hook to register with `indexing::Indexing::listen`, and the
    /// receiver [`Recovery::new`] takes.
    pub(crate) fn channel() -> (Arc<dyn Listener>, watch::Receiver<Option<BlockRef>>) {
        let (announce, progress) = watch::channel(None);
        (Arc::new(Self(announce)), progress)
    }
}

impl Listener for CommitSignal {
    fn committed(&self, commit: &BlockCommit) {
        self.0.send_replace(Some(commit.block.clone()));
    }
}

/// Resolves every retained transaction until it is present or provably
/// absent, then releases its sender.
pub(crate) struct Recovery {
    submitter: Arc<dyn Submitter>,
    checkpoint: Arc<dyn Checkpoint>,
    presence: Arc<dyn Presence>,
    network: Network,
    progress: watch::Receiver<Option<BlockRef>>,
    reservations: Reservations,
}

/// What the submit stage hands an unknown outcome to.
pub(crate) trait Settler: Send + Sync {
    /// Settles `transaction`; returns once its sender is released.
    fn settle(&self, transaction: SignedTransaction) -> BoxFuture<'_, ()>;
}

impl Recovery {
    pub(crate) fn new(
        submitter: Arc<dyn Submitter>,
        checkpoint: Arc<dyn Checkpoint>,
        presence: Arc<dyn Presence>,
        network: Network,
        progress: watch::Receiver<Option<BlockRef>>,
        reservations: Reservations,
    ) -> Self {
        Self {
            submitter,
            checkpoint,
            presence,
            network,
            progress,
            reservations,
        }
    }

    async fn settle(&self, transaction: &SignedTransaction) {
        let mut progress = self.progress.clone();
        let mut previous = None;
        let mut backoff = Backoff::default();
        loop {
            let verdict = self.inspect(transaction).await;
            if matches!(verdict, Verdict::Present | Verdict::Absent) {
                self.reservations.release(&transaction.from);
                return;
            }
            let checkpoint = verdict.checkpoint().cloned();
            if checkpoint != previous {
                previous = checkpoint;
                backoff.reset();
            }
            let delay = backoff.next();
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                Ok(()) = progress.changed() => {
                    if *progress.borrow_and_update() != previous {
                        backoff.reset();
                    }
                }
            }
        }
    }

    async fn inspect(&self, transaction: &SignedTransaction) -> Verdict {
        let known = self
            .submitter
            .known(&transaction.id, transaction.min_slot)
            .await;
        if known.is_ok_and(|known| known) {
            return Verdict::Present;
        }
        let checkpoint = match self.checkpoint.checkpoint(&self.network).await {
            Ok(Some(checkpoint)) => checkpoint,
            _ => return Verdict::Retry(None),
        };
        match self.scan(transaction, &checkpoint).await {
            Scan::Present => return Verdict::Present,
            Scan::Unstable => return Verdict::Retry(Some(checkpoint)),
            Scan::Exhausted => {}
        }
        let Ok(height) = self.submitter.height(transaction.min_slot).await else {
            return Verdict::Retry(Some(checkpoint));
        };
        let last_valid = transaction.lifetime.last_valid_block_height();
        if transaction.lifetime.expired(height) && checkpoint.height.0 >= last_valid {
            Verdict::Absent
        } else {
            Verdict::Retry(Some(checkpoint))
        }
    }

    /// Whether the index holds the transaction, read at `checkpoint`: a read
    /// at any other line proves nothing.
    async fn scan(&self, transaction: &SignedTransaction, checkpoint: &BlockRef) -> Scan {
        let id = transaction.id.canonical(&self.network);
        match self.presence.landed(&id).await {
            Ok(landed) if landed.present => return Scan::Present,
            Ok(landed) if landed.checkpoint.as_ref() == Some(checkpoint) => {}
            _ => return Scan::Unstable,
        }
        match self.checkpoint.checkpoint(&self.network).await {
            Ok(current) if current.as_ref() == Some(checkpoint) => Scan::Exhausted,
            _ => Scan::Unstable,
        }
    }
}

impl Settler for Recovery {
    fn settle(&self, transaction: SignedTransaction) -> BoxFuture<'_, ()> {
        Box::pin(async move { Self::settle(self, &transaction).await })
    }
}

enum Verdict {
    Present,
    Absent,
    Retry(Option<BlockRef>),
}

impl Verdict {
    const fn checkpoint(&self) -> Option<&BlockRef> {
        match self {
            Self::Retry(checkpoint) => checkpoint.as_ref(),
            Self::Present | Self::Absent => None,
        }
    }
}

enum Scan {
    Present,
    Exhausted,
    Unstable,
}

/// Half a second, doubling to ten seconds.
struct Backoff(Duration);

impl Default for Backoff {
    fn default() -> Self {
        Self(Duration::from_millis(500))
    }
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let current = self.0;
        self.0 = self.0.saturating_mul(2).min(Duration::from_secs(10));
        current
    }

    fn reset(&mut self) {
        self.0 = Duration::from_millis(500);
    }
}
