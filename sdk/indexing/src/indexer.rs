//! What an application calls to read an index.

use std::sync::Arc;

use crate::{
    Backfilling, BlockRef, BoxFuture, IndexError, Listener, Network, SyncStatus, Watching,
};

/// Reads how far a network is indexed.
pub trait Checkpoint: Send + Sync {
    /// The last committed block of `network`, or `None` before the first commit.
    ///
    /// This is the network's confirmed line, not its tip: blocks above it are
    /// held in memory and reported separately.
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>>;
}

/// What an index *is*: it observes **one** network, it advances it, and it says
/// where it got to.
///
/// **One network, one status.** Every indexer this workspace ships is one
/// chain's [`Service`](crate::Service) over one `(chain, network)`, so the
/// contract says so: a pass answers with the single [`SyncStatus`] of the single
/// network [`Self::network`] names. Many networks is what [`crate::Index`] is
/// for, and it aggregates these rather than being one of them — so nothing has
/// to check that a child reported each of its networks exactly once, and no
/// driver holds a vector of health rows per task.
///
/// **The whole of it, and the minimum.** Everything here is answerable by a
/// store that implements [`Repository`](crate::Repository) — registration and
/// committing — and nothing here reads a balance, a history page or an output.
/// That is the whole point of the split: a third party whose sink answers no
/// query at all still gets an index it can observe wallets with and drive
/// (`tests/minimum_contract.rs`), and a consumer that does read a query holds
/// the value that answers it ([`crate::ObservedHistory`],
/// [`crate::ObservedOutputs`], [`crate::Balances`]) rather than reaching for it
/// through the thing doing the indexing. There is no wider indexer trait above
/// this one; there was, carrying [`crate::Balances`], and it was a courier —
/// nothing that held it read the balance it forwarded.
///
/// Implemented by [`crate::Service`], which is what each chain's own
/// constructor returns.
pub trait Indexing: Backfilling + Checkpoint + Watching {
    /// The one `(chain, network)` network this indexer synchronizes and answers
    /// for.
    fn network(&self) -> &Network;

    /// Notifies `listener` after each block this indexer commits. A list, not
    /// a slot; a composer forwards to every unit.
    fn listen(&self, listener: Arc<dyn Listener>);

    /// Advances this indexer's network, against its own watch list.
    ///
    /// **Bounded.** One pass does the work its [`crate::SyncPolicy`] allows and
    /// returns; it is not a loop, and nothing here schedules, retries or waits.
    /// A caller drives it — in its own loop, from a cron, or through the
    /// optional `crate::Supervisor`, which calls this and owns no second
    /// implementation of scanning or committing.
    ///
    /// The list is read during the pass rather than passed in, so it is never
    /// older than the tip the pass indexes towards: an address watched between
    /// the two reads has a birthday the pass's own blocks already cover.
    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>>;
}
