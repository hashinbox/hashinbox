//! The spendable set this network's index answers: the repository's unspent
//! rows, folded over the blocks it still holds above the confirmed line.
//!
//! Beside its parent rather than inside a folder of its own, for the reason
//! `scripts/check-chains.sh` check 11 states: a directory holding one file is
//! not a boundary. `pub(super)` therefore still means *visible to
//! [`crate::reads`]*, which is what [`crate::Service::outputs`] needs to build
//! one of these.
//!
//! **Its own subject, which is why it is its own file.** A history page and a
//! balance answer with a [`crate::Completeness`] and let the caller decide what
//! a partial answer is worth. This read cannot: its consumer is coin selection,
//! an output it offers becomes a transaction input, and the two refusals below
//! — rows not yet proven unspent, and a cursor folded over a different overlay
//! — are the whole of what it adds to a store's page. Nothing about them is a
//! statement about how much of an address's history is indexed, which is the
//! only question its two siblings answer.

use std::sync::Arc;

use crate::{
    BoxFuture, IndexError, IndexErrorKind, OutputCursor, OutputPage, OutputRequest, OutputStore,
    overlay::{Frontier, LiveChanges},
    watches::Watches,
};

/// One network's spendable set: the repository's own unspent rows, minus what
/// the blocks above its confirmed line have already spent, plus what they have
/// created.
///
/// The same three dependencies [`super::ObservedHistory`] names, for the same
/// reason, and built the same way ([`crate::Service::outputs`]). A repository
/// answers rows at a checkpoint; the two gates below and the overlay fold are
/// what the index adds, and they are here rather than on [`crate::Service`]
/// because a store's rows are not an indexer's to answer for.
///
/// **The one read whose pages must agree with each other.**
/// Outputs the pending blocks spent are removed, and outputs they created are
/// appended to the last page, so paging stays stable while a wallet can still
/// spend its own change at depth one. Those two halves come from one overlay
/// sample, which is what makes a *single* page consistent
/// (`crate::overlay::Sample::above`) — and says nothing about the next one.
///
/// It has to, because the consumer is coin selection. `UtxoReader` pages until
/// the set is exhausted and spends the union, so a pending block admitted
/// between two pages could hand a caller an input it must not spend *and* the
/// change that exists only because that input was spent: the spend is applied
/// to the page it lands on, and an output already returned by an earlier page
/// is out of reach of `retain`. The durable checkpoint cannot catch it — a
/// block held above the line does not move the checkpoint — so the cursor
/// carries the overlay's revision and a later page is folded over the same
/// blocks or refused.
///
/// Refusing, rather than pinning the sample and serving it for the rest of the
/// read, for two reasons. The sample is not reconstructible: the overlay's own
/// `unwind` discards held blocks and its `replace` rewrites their facts, so by
/// page two the blocks page one folded may not exist anywhere. Keeping a copy
/// per outstanding cursor would be shared mutable state with a lifetime and an
/// eviction rule — machinery to coordinate two readers — and it would answer
/// with a spendable set a reorg has already discarded. A refusal hands back the
/// newest truth instead, and costs a caller nothing new: it is the same
/// retryable `Conflict` the store already raises when the checkpoint moves
/// under a paginated read, which every caller of this port already restarts on.
///
/// The same refusal, for the same reason, settles the *other* way a page can be
/// a lie: rows that are unspent only as far as the index has read (the private
/// `spendable` gate below).
pub struct ObservedOutputs<R> {
    pub(super) repository: R,
    pub(super) pending: Arc<Frontier>,
    pub(super) watches: Arc<Watches>,
}

impl<R: OutputStore> OutputStore for ObservedOutputs<R> {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            let address = request.address.clone();
            let sample = self.pending.sampled()?;
            let revision = sample.revision;
            Self::resumable(request.after.as_ref(), revision)?;
            let mut page = self.repository.list(request).await?;
            let backfilling = self
                .watches
                .backfilling(&address, page.checkpoint.as_ref())?;
            Self::spendable(backfilling)?;
            let pending = sample.above(page.checkpoint.as_ref());
            let changes = LiveChanges::fold(&address, &pending);
            page.outputs
                .retain(|output| !changes.spent.contains(&output.key()));
            if page.next.is_none() {
                page.outputs.extend(changes.created);
            }
            // Every cursor this read hands out names the overlay it was folded
            // over, which is the only thing the next page has to be checked
            // against: the store's own position and checkpoint it already
            // carries.
            page.next = page.next.map(|next| OutputCursor {
                revision: Some(revision),
                ..next
            });
            Ok(page)
        })
    }
}

impl<R> ObservedOutputs<R> {
    /// Refuses the read while the rows it would answer from are still arriving.
    ///
    /// **The one read that cannot be weakened instead.** A balance and a history
    /// page carry [`crate::Completeness`], so they answer honestly while an
    /// inspection is outstanding and the caller decides what a partial answer is
    /// worth. An [`OutputPage`] carries no such claim and could not use one: its
    /// consumer is coin selection, which pages the set and spends the union
    /// (`UtxoReader`), so an output offered here becomes a transaction input. An
    /// unspent row at block 0 is only unspent *as far as has been read*, and
    /// until the inspection reaches the checkpoint the page was read at, the
    /// blocks between may hold the very spend that settles it. The rows exist,
    /// the checkpoint is real, and the two together are still a lie, which
    /// leaves refusing as the only honest answer.
    ///
    /// Taken on the checkpoint the store's own answer carried, exactly as
    /// [`crate::History`] and [`crate::Balances`] take the same fact, so a gate
    /// and the rows it guards can never be read at two different lines.
    ///
    /// A retryable `Conflict`: it is the refusal every caller of this port
    /// already restarts on, and the wallet layer maps it to a retryable
    /// `Unavailable` before a send ever forms
    /// (`sdk/chains/bitcoin/src/sender/outbox.rs`, `prepare`). It clears the
    /// moment the catch-up reaches the line, which is what the backfill half of
    /// each pass is doing.
    fn spendable(backfilling: bool) -> Result<(), IndexError> {
        if !backfilling {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::Conflict,
            "spendable outputs are not readable while this address's own history \
             is still being inspected: an unspent row cannot be proven unspent \
             below the checkpoint it was read at",
            true,
        ))
    }

    /// Refuses a later page that would be folded over a different pending set
    /// than the page before it.
    ///
    /// The cursor states which overlay it was folded over
    /// ([`OutputCursor::revision`]), so this is that value against the one now
    /// held: equal, and the rest of the read continues over the same blocks;
    /// different, and it is the retryable `Conflict` the caller already restarts
    /// on when the checkpoint moves under a paginated read.
    ///
    /// A cursor carrying no revision at all was not issued by this index. It is
    /// refused terminally rather than retried, because retrying cannot make a
    /// foreign cursor belong here, and serving it would page the store over an
    /// overlay of the index's own choosing.
    fn resumable(after: Option<&OutputCursor>, revision: u64) -> Result<(), IndexError> {
        let Some(cursor) = after else {
            return Ok(());
        };
        let Some(folded) = cursor.revision else {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "an output cursor does not belong to this index",
                false,
            ));
        };
        if folded == revision {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::Conflict,
            "pending blocks changed during pagination",
            true,
        ))
    }
}
