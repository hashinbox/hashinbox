//! Settling an unknown submission outcome in the background.
//!
//! The sender stays reserved until one of two proofs holds: the node or the
//! index shows the transaction (present), or its blockhash expired and the
//! index covers that height without it (absent). Until then the check
//! repeats with a doubling backoff, restarted whenever the index moves.

use std::{sync::Arc, time::Duration};

use base::{BlockRef, BoxFuture};
use indexing::History;
use indexing::{
    AssetId, BlockCommit, Checkpoint, HistoryPosition, Listener, Network, Page, Subject,
};
use tokio::sync::watch;

use super::reservation::Reservations;
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
    history: Arc<dyn History>,
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
        history: Arc<dyn History>,
        network: Network,
        progress: watch::Receiver<Option<BlockRef>>,
        reservations: Reservations,
    ) -> Self {
        Self {
            submitter,
            checkpoint,
            history,
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

    /// Pages through the sender's indexed SOL history at one stable
    /// checkpoint: the sender pays every fee, so its transaction is there
    /// even when it moved no SOL.
    async fn scan(&self, transaction: &SignedTransaction, checkpoint: &BlockRef) -> Scan {
        let address = transaction.from.canonical(&self.network);
        let id = base::TransactionId::from(transaction.id);
        let subject = Subject {
            address: address.clone(),
            asset: AssetId::native(self.network.chain.clone()),
        };
        let mut after: Option<HistoryPosition> = None;
        loop {
            let page = Page {
                after: after.clone(),
                limit: 100,
            };
            let page = match self.history.history(&subject, page).await {
                Ok(page) if page.checkpoint.as_ref() == Some(checkpoint) => page,
                _ => return Scan::Unstable,
            };
            let seen = page
                .transactions
                .iter()
                .chain(&page.pending)
                .any(|observed| observed.transaction.id.id == id);
            if seen {
                return Scan::Present;
            }
            match page.next {
                Some(next) if after.as_ref() != Some(&next) => after = Some(next),
                Some(_) => return Scan::Unstable,
                None => break,
            }
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::native::rpc::script::{Script, Scripted};
    use crate::sender::stub::{Index, checkpoint, key, network, signed};

    fn recovery(script: &Script, index: Index, reservations: Reservations) -> Recovery {
        let index = Arc::new(index);
        let (_announce, receiver) = CommitSignal::channel();
        Recovery::new(
            Arc::new(script.client()),
            index.clone(),
            index,
            network(),
            receiver,
            reservations,
        )
    }

    /// The recovery hook is one entry of a listener list, so a caller's own
    /// listener is announced beside it rather than displaced by it — which is
    /// what a single observer slot did.
    #[test]
    fn progress_announces_each_block_beside_the_callers_listener() {
        #[derive(Default)]
        struct Seen(std::sync::Mutex<Vec<BlockRef>>);

        impl Listener for Seen {
            fn committed(&self, commit: &BlockCommit) {
                self.0.lock().unwrap().push(commit.block.clone());
            }
        }

        let seen = Arc::new(Seen::default());
        let (announce, progress) = CommitSignal::channel();
        let block = checkpoint(7);
        let commit = BlockCommit {
            network: network(),
            block: block.clone(),
        };
        for listener in [announce, seen.clone() as Arc<dyn Listener>] {
            listener.committed(&commit);
        }
        assert_eq!(*progress.borrow(), Some(block.clone()));
        assert_eq!(*seen.0.lock().unwrap(), [block]);
    }

    #[test]
    fn backoff_doubles_to_ten_seconds_and_resets_on_progress() {
        let mut backoff = Backoff::default();
        for expected in [500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000] {
            assert_eq!(backoff.next(), Duration::from_millis(expected));
        }
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_millis(500));
    }

    #[tokio::test]
    async fn status_presence_releases_exactly_the_ambiguous_sender() {
        let key = key(7);
        let transaction = signed(&key, 3, 11);
        let rpc = Script::exchanges([(
            "getSignatureStatuses",
            json!([[transaction.id.to_string()], {"searchTransactionHistory":true}]),
            json!({
                "context": {"slot": 15},
                "value": [{
                    "slot": 12,
                    "confirmations": null,
                    "err": null,
                    "confirmationStatus": "finalized",
                }],
            }),
        )]);
        let reservations = Reservations::default();
        reservations
            .admit(std::slice::from_ref(key.address()), false)
            .expect("reservation")
            .settlement()
            .retain(key.address());
        Settler::settle(
            &recovery(&rpc, Index::default(), reservations.clone()),
            transaction,
        )
        .await;
        reservations
            .admit(std::slice::from_ref(key.address()), false)
            .expect("presence releases the sender");
        rpc.exhausted();
    }

    #[tokio::test]
    async fn terminal_absence_requires_expiry_coverage_and_one_stable_exhausted_scan() {
        let transaction = signed(&key(7), 3, 11);
        let rpc = Script::exchanges([
            (
                "getSignatureStatuses",
                json!([[transaction.id.to_string()], {"searchTransactionHistory":true}]),
                json!({"context":{"slot":15},"value":[null]}),
            ),
            (
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":11}]),
                json!(45),
            ),
        ]);
        let index = Index {
            checkpoint: Some(checkpoint(44)),
            page_checkpoint: Some(checkpoint(44)),
        };
        let recovery = recovery(&rpc, index, Reservations::default());
        assert!(matches!(
            recovery.inspect(&transaction).await,
            Verdict::Absent
        ));
        rpc.exhausted();
    }

    #[tokio::test]
    async fn checkpoint_conflict_keeps_absence_unresolved_without_height_rpc() {
        let transaction = signed(&key(7), 3, 11);
        let rpc = Script::exchanges([(
            "getSignatureStatuses",
            json!([[transaction.id.to_string()], {"searchTransactionHistory":true}]),
            json!({"context":{"slot":15},"value":[null]}),
        )]);
        let index = Index {
            checkpoint: Some(checkpoint(44)),
            page_checkpoint: Some(checkpoint(43)),
        };
        let recovery = recovery(&rpc, index, Reservations::default());
        assert!(matches!(
            recovery.inspect(&transaction).await,
            Verdict::Retry(Some(_))
        ));
        rpc.exhausted();
    }
}
