//! What the [`Indexer`](crate::Indexer) moves over a network's blocks: one
//! kind of index each, a wallet's ([`WalletIndex`]) and a sent
//! transaction's ([`TransactionIndex`]), with what both share:
//! [`Progress`], [`Indexed`], [`IntoIndex`].

mod transaction;
mod wallet;

pub use transaction::{
    Confirmed, Confirmer, NoTransactionIndexes, TransactionIndex, TransactionIndexStore,
    TransactionIndexTarget, TransactionIndexer, TransactionStatus,
};
pub use wallet::{Birthday, WalletIndex, WalletIndexStore, WalletIndexTarget, WalletIndexer};

use crate::{BlockHeader, Future};

/// Items a kind settles per step at most ([`crate::Indexes::close`]); a
/// jump or a give-up takes them all.
const CLOSES: usize = 8;

/// Whether an index is scanned; set only on events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexState {
    /// Scanned: the state it is made in, and resumed to.
    #[default]
    Indexing,
    /// Skipped until resumed; resumed, it backfills what it missed.
    Paused,
    /// Skipped: the node no longer serves a block it needs, or it was given
    /// up.
    NoData,
}

/// How far an index has read, and in what state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Scanned through this position; `None` before the first.
    pub indexed_at: Option<u64>,
    /// Whether it is scanned.
    pub state: IndexState,
    /// Why it is [`IndexState::NoData`]; `None` otherwise.
    pub reason: Option<String>,
}

/// What the indexer needs of either kind.
pub trait Indexed {
    /// The first block it reads: a wallet's `birthday`, a transaction's
    /// `broadcasted_at`.
    fn start(&self) -> u64;

    /// How far it has read.
    fn progress(&self) -> &Progress;

    /// The next block to read: after `indexed_at`, or [`Self::start`] while
    /// it has not started; `None` past the last representable one.
    fn next(&self) -> Option<u64> {
        self.progress()
            .indexed_at
            .map_or(Some(self.start()), |position| position.checked_add(1))
    }

    /// Whether it is scanned through `last`, the network's last kept block.
    fn caught_up(&self, last: Option<&BlockHeader>) -> bool {
        match (self.next(), last) {
            (None, _) | (Some(_), None) => true,
            (Some(next), Some(last)) => next > last.position,
        }
    }
}

/// Something that becomes an index of kind `I`: a target, which the chain
/// fills in, or a finished index, its own target.
pub trait IntoIndex<I>: Send + Sync {
    /// The index.
    ///
    /// # Errors
    ///
    /// When the chain cannot answer, or refuses the target.
    fn index(&self) -> Future<'_, anyhow::Result<I>>;
}

/// What one kind's `skip` did ([`crate::Indexes::skip`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// An unstarted item now starts at the first block served.
    pub moved: bool,
    /// A started item is [`IndexState::NoData`].
    pub stopped: bool,
}

impl Skipped {
    /// What `self` and `other` did together.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        Self {
            moved: self.moved || other.moved,
            stopped: self.stopped || other.stopped,
        }
    }
}

/// The lowest [`Indexed::next`] of the indexing `items` at or above `from`
/// and at or below `through` (`None`: no bound): a store's default
/// `find_next`.
pub(crate) fn lowest_next<I: Indexed>(items: &[I], from: u64, through: Option<u64>) -> Option<u64> {
    items
        .iter()
        .filter(|item| item.progress().state == IndexState::Indexing)
        .filter_map(Indexed::next)
        .filter(|next| *next >= from && through.is_none_or(|through| *next <= through))
        .min()
}

/// The indexing `items` whose [`Indexed::next`] is below `below`, furthest
/// behind first, at most `limit`: a store's default `find_behind`.
pub(crate) fn behind<I: Indexed>(items: Vec<I>, below: u64, limit: usize) -> Vec<I> {
    let mut behind: Vec<I> = items
        .into_iter()
        .filter(|item| item.progress().state == IndexState::Indexing)
        .filter(|item| item.next().is_some_and(|next| next < below))
        .collect();
    behind.sort_by_key(Indexed::next);
    behind.truncate(limit);
    behind
}
