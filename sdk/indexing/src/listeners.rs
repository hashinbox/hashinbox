//! The post-commit hook: what one committed block changed, who is told about
//! it, and the list one service tells.
//!
//! A list rather than a slot, and synchronous rather than awaited: a hook that
//! cannot await cannot slow a pass by awaiting, and the dispatch keeps its
//! allocation-free shape by sharing the list rather than copying it
//! (`docs/design/030 §8 (ex-029)` section 6.3).

use std::sync::{Arc, RwLock};

use crate::{BlockRef, CanonicalTransaction, Network, OutputChanges, OutputKey};

/// What one committed block changed, delivered after it was durably stored.
///
/// Only blocks that actually changed the index are reported: a block the store's
/// own line already covers is never presented again, so a restart that finds its
/// last commit landed does not re-notify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockCommit {
    /// The network that committed the block.
    pub network: Network,
    /// The committed block; now the network's checkpoint.
    pub block: BlockRef,
    /// Transactions in this block that touched a watched address. Empty when
    /// the block was indexed and matched nothing.
    pub transactions: Vec<CanonicalTransaction>,
    /// What became spendable and what was spent, for the same addresses.
    pub outputs: OutputChanges,
    /// Required spends committed without a matching indexed output: one
    /// address's history is provably short
    /// ([`IndexStore::commit`](crate::IndexStore::commit)).
    pub unmatched_spends: Vec<OutputKey>,
}

/// Notified after each block an index commits.
///
/// Registered with [`Indexing::listen`](crate::Indexing::listen), called once per
/// committed block in block order, on the synchronizing task, holding no lock
/// the caller can reach. It returns nothing and cannot fail: the block is
/// already durable, so there is nothing to refuse and nothing to report. Do no
/// I/O here — send to a channel. A veto belongs in a
/// [`IndexStore`](crate::IndexStore) implementation, which is the only thing
/// that runs inside the commit.
///
/// Delivery is **at most once, and never retracted**. A listener that must not
/// miss a block reads [`Checkpoint`](crate::Checkpoint) when it starts and
/// catches up through the repository's own history
/// ([`ConfirmedHistory`](crate::ConfirmedHistory)); this is a wake-up, not a
/// log.
pub trait Listener: Send + Sync {
    /// Called once per committed block, after the store returned.
    fn committed(&self, commit: &BlockCommit);
}

/// Every [`Listener`] registered on one service, in registration order.
///
/// The list is shared rather than owned by the lock: registration replaces it,
/// and a dispatch takes a share of whatever it was at that moment. That is what
/// lets [`Self::announce`] release the lock before it calls anything, which is
/// the whole of why a callback may re-enter [`Listeners::add`].
#[derive(Default)]
pub(crate) struct Listeners(RwLock<Arc<[Arc<dyn Listener>]>>);

impl Listeners {
    /// Adds `listener` to the end of the list.
    ///
    /// Copies the list to extend it, which costs one allocation per
    /// registration — a composition-time call, and the price of a dispatch that
    /// allocates nothing per block and holds nothing across a callback.
    ///
    /// A poisoned list is dropped rather than propagated: registration is a
    /// composition-time call with no error channel, and losing a notification
    /// never loses a durable fact.
    pub(crate) fn add(&self, listener: Arc<dyn Listener>) {
        if let Ok(mut listeners) = self.0.write() {
            let mut extended = listeners.to_vec();
            extended.push(listener);
            *listeners = extended.into();
        }
    }

    /// Whether anything is listening, so a pass copies a commit's facts only
    /// when something will read them.
    pub(crate) fn listening(&self) -> bool {
        self.0.read().is_ok_and(|listeners| !listeners.is_empty())
    }

    /// Tells every listener about one committed block, oldest first, after the
    /// store returned. It cannot fail, so nothing here can abort a pass.
    ///
    /// **The lock is released before the first callback runs.** A callback is a
    /// consumer's own code and may call anything, including
    /// [`Indexing::listen`](crate::Indexing::listen); holding the list across
    /// the loop turned that plain call into a deadlock of the whole pass, with
    /// no diagnostic. Taking a share of the list instead costs one refcount and
    /// nothing else.
    ///
    /// The list a block is announced to is the one registration left at the
    /// moment that block's announcement began, so a listener registered during
    /// a dispatch — by another thread, or by a callback in this very loop —
    /// joins at the next block. Delivery stays at most once and never
    /// retracted; a listener that must not miss a block still starts from its
    /// [`Checkpoint`](crate::Checkpoint), as [`Listener`] says.
    pub(crate) fn announce(&self, commit: &BlockCommit) {
        let Ok(listeners) = self.0.read().map(|listeners| Arc::clone(&listeners)) else {
            return;
        };
        for listener in listeners.iter() {
            listener.committed(commit);
        }
    }
}
