//! The durable record of what an index observes, and how far it got.
//!
//! The index owns its watch list and keeps it hot in memory, because the
//! admission handshake captures it synchronously; this is where that list
//! survives a restart. [`crate::Watching::watch`] writes the observations and a
//! [`crate::Indexer`] reads them back, with their recorded progress, once
//! before its first pass.
//!
//! One read, not two. A watch and how far it has been inspected are one fact
//! about one observation, so they come back together from one snapshot
//! ([`WatchStore::all`]): a store that answered them separately could hand back
//! progress earned under a registration it no longer holds, and the caller
//! would have no way to tell.

use crate::{BlockPosition, BlockRef, BoxFuture, IndexError, IndexErrorKind, Network, Watch};

/// One observed watch, and the block through which it has been inspected.
///
/// The pair is the unit [`WatchStore::all`] returns, because it is the unit a
/// restart needs: the observation says what to inspect, and the progress says
/// what has already been inspected for it. Neither is usable without the other
/// — progress without its watch claims a range for nothing, and a watch without
/// its progress re-derives a range the store can already prove was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchProgress {
    /// The observation, exactly as it was registered.
    pub watch: Watch,
    /// The block through which **every applicable block from the watch's
    /// birthday has been inspected for it** — blocks with no matching activity
    /// included — or `None` when nothing has been inspected for it yet.
    ///
    /// Contiguous by construction, which is what lets one reference describe the
    /// whole of what has been read: there is no second position and no hole
    /// between them. A watch born at 20 and inspected through 25 resumes at 26;
    /// a watch born at 22 with `None` resumes at 22, and registering it resets
    /// nothing for the first.
    ///
    /// It is a hash-verified [`BlockRef`] rather than a position, and it is
    /// always at or below the network's checkpoint
    /// ([`crate::IndexStore::checkpoint`]) — a store refuses a claim above the
    /// line. One `canonical_at` on the checkpoint therefore vouches for every
    /// one of these at once, because a confirmed block never reverts
    ///; verifying them one by one would be an RPC per watch
    /// per pass for no further proof.
    pub indexed_through: Option<BlockRef>,
}

impl WatchProgress {
    /// An observation with nothing inspected for it yet.
    #[must_use]
    pub const fn unread(watch: Watch) -> Self {
        Self {
            watch,
            indexed_through: None,
        }
    }

    /// The first block still to be inspected for this observation, or `None`
    /// when no further block can ever be required.
    ///
    /// Native positions may be sparse — Solana's are slots, most of which hold
    /// no block — so this is where traversal *resumes*, not necessarily a block
    /// that exists. A pass inspects the next block the source actually produces
    /// at or above it, and records that block; the positions in between were
    /// never produced, so nothing was skipped.
    ///
    /// `None` only for a watch inspected through the last representable
    /// position, which is a source reporting a block at the numeric boundary.
    /// Such a watch owes nothing further, rather than re-reading the same range
    /// on every pass for the life of the process.
    #[must_use]
    pub fn next_required(&self) -> Option<BlockPosition> {
        match &self.indexed_through {
            Some(block) => block.position.checked_successor(),
            None => Some(self.watch.start),
        }
    }

    /// Whether this observation is inspected for blocks arriving at the front of
    /// the chain, and — the same fact — whether its reads may claim to be
    /// complete.
    ///
    /// **This is the design's central trade, and it is this function.** Two
    /// behaviours are possible for an observation whose own history is not yet
    /// read up to the confirmed line:
    ///
    /// - **Chosen: it does not join the front.** It is inspected only for the
    ///   blocks it still owes, from its birthday upwards, until
    ///   [`Self::indexed_through`] reaches the checkpoint; then it joins. What
    ///   that buys is contiguity — one reference describes the whole of what has
    ///   been inspected, so nothing has to represent, migrate or reason about a
    ///   hole. What it costs is latency: a wallet imported with an old birthday
    ///   does not see a payment arriving *now* until its catch-up reaches the
    ///   present.
    /// - **Not chosen: it joins immediately.** A payment arriving now is seen at
    ///   once while older history fills in behind it. Its coverage is then two
    ///   ranges with a gap between them, which one contiguous reference cannot
    ///   describe: it needs a second position, a reader that knows about the gap,
    ///   and a rule for closing it.
    ///
    /// Flipping the trade is changing this one comparison. Everything that
    /// depends on it — the selection a pass inspects each block for, and what
    /// a store may claim or must refuse on a balance, a history page and a
    /// spendable set ([`Inspection`](crate::Inspection)) — asks here rather
    /// than deciding for itself, so the two can never disagree about what
    /// "still catching up" means. A watch that is still catching up **must**
    /// read as incomplete; a wallet reading `Complete` with a short history
    /// would be worse than the latency it replaced.
    ///
    /// A network with no committed block has no line to be behind, so nothing is
    /// owed below it.
    #[must_use]
    pub fn caught_up(&self, checkpoint: Option<&BlockRef>) -> bool {
        match (self.next_required(), checkpoint) {
            (None, _) | (Some(_), None) => true,
            (Some(next), Some(line)) => next > line.position,
        }
    }
}

/// The durable list of what one index observes.
///
/// One half of the minimum an index sink must implement ([`crate::Repository`]);
/// the other is [`crate::IndexStore`]. Nothing here reads a balance, a history
/// page or an output: those are consumer capabilities a repository may also
/// offer, and an index neither calls nor requires them.
pub trait WatchStore: Send + Sync {
    /// Atomically records every observation in `watches`, with no progress.
    ///
    /// **Atomic**: either every watch in the batch is registered or none is, so
    /// a caller that adds a wallet's native and token observations together can
    /// never end up with one of them. A storage error or a cancelled future can
    /// still leave the outcome uncertain — retry rather than assuming rollback,
    /// which is what makes the next rule load-bearing.
    ///
    /// **An exact repeat is accepted and preserves existing progress.** A watch
    /// whose identity, address, asset and birthday are all the ones already
    /// stored is the same observation, so re-presenting it is how an uncertain
    /// `add` is resolved; it must not reset what has been inspected for it.
    ///
    /// **A conflicting definition fails the whole batch.** A stored identity
    /// under another address, asset or birthday, or a stored `(address, asset)`
    /// pair under another identity, is a different observation wearing a name
    /// that is taken, and accepting it would silently retarget progress earned
    /// by the other one.
    ///
    /// **An archived row is observed again** when the watch names its own
    /// identity and pair: from the birthday given, with nothing inspected and
    /// its total seeded from the facts already stored, in the same
    /// transaction. That includes a row a [`crate::WalletRegistry`] filed
    /// unobserved. An archived pair stays reserved for its identity, so
    /// another identity is refused as above.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::Conflict`], not retryable, for a conflicting
    /// definition — including an identity another network of the same store
    /// holds — [`crate::IndexErrorKind::InvalidRequest`] for a blank address, an asset
    /// of another chain than its address, or a batch spanning more than one
    /// network, and the store's own error.
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>>;

    /// Every observation of exactly `network`, with its recorded progress, from
    /// **one consistent snapshot**.
    ///
    /// The snapshot is the contract, not an implementation note: the index
    /// pairs each watch with its progress and then owns that pairing for the
    /// rest of the process, so two reads at two instants could hand it a watch
    /// beside another watch's progress. A SQL store reads both inside one
    /// transaction.
    ///
    /// **No transaction history is loaded.** This is the list of what to
    /// observe and how far observation got, which is bounded by the watch
    /// count; a store that walked history to answer it would make every restart
    /// cost the whole index.
    ///
    /// Only `network`'s rows: one store serves every configured network, and
    /// each network's index reads its own.
    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>>;

    /// Archives `watch`: it leaves [`Self::all`], and the progress and total
    /// recorded for it go, so nothing inherits either.
    ///
    /// **By identity.** The row archived is the one registered under
    /// `watch.id` in `network`; the `(address, asset)` pair `watch` names is
    /// not a second key, so a pair another identity holds is never touched by
    /// this call.
    ///
    /// The durable half of [`crate::Watching::unwatch`]. The row itself stays,
    /// with its birthday and whatever an application filed beside it
    /// ([`crate::WalletRegistry`]), and its pair stays reserved for its
    /// identity: [`Self::add`] under the same identity and pair observes it
    /// again. The history, credits and live outputs of the address are
    /// untouched, which is what makes the withdrawal reversible.
    ///
    /// Progress belongs to the observation and goes with it. Another
    /// observation of the same address keeps its own, which it earned under its
    /// own birthday — progress is per observation now, so there is no shared row
    /// for a withdrawal to orphan.
    ///
    /// Idempotent: a watch this store does not hold, or holds archived, is not
    /// an error, because an observation nothing scans is the state the caller
    /// asked for; an archived row keeps the time it was first archived at.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::NetworkMismatch`] when `watch` is not of
    /// `network`, and the store's own error.
    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>>;
}

/// A definition refused because another observation holds one of its names.
///
/// An identity under another `(address, asset)` pair or birthday, a pair under
/// another identity, or an identity another network holds. **Not retryable**:
/// the stored row is the caller's own earlier registration, not a race, so
/// presenting the same definition again can only be refused again. Both shipped
/// stores answer it with this one value so they cannot disagree about it.
#[cfg(any(feature = "sqlite", feature = "memory"))]
pub(crate) fn redefinition() -> IndexError {
    IndexError::new(
        crate::IndexErrorKind::Conflict,
        "an address is already registered for this identity or network",
        false,
    )
}

/// A watch store held behind a reference count is a watch store.
///
/// What makes a custom sink usable without being [`Clone`]. An index clones its
/// repository handle when it is assembled, so a sink that owns a connection, a
/// channel or anything else that cannot be duplicated is passed as
/// `Arc<MySink>` and this forwards every call to the one instance.
impl<T: WatchStore + ?Sized> WatchStore for std::sync::Arc<T> {
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        (**self).add(watches)
    }

    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        (**self).all(network)
    }

    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        (**self).forget(network, watch)
    }
}

/// How far one address has been inspected, as one snapshot holds it: every
/// observation of the address, and the confirmed line of the same read.
///
/// **What keeps a read honest about rows that are not there yet.** A store
/// answers from the rows it holds, and a backfill exists precisely because some
/// are missing: the range between an observation's recorded progress and the
/// line has not been read for the address. Only the store can say so without a
/// second read, because the observations and the rows are in one snapshot. So
/// each shipped store loads this beside the rows it answers with and asks it
/// what the answer may claim.
///
/// **Scoped to the address, not to the observation**, and deliberately. One
/// inspection of an address serves every asset observed at it
/// ([`crate::BlockInterpreter::inspect`] takes addresses and no asset), so the
/// rows a token read folds are the rows its native sibling's catch-up is still
/// inserting. The conservative answer is the true one.
///
/// Shared by the two shipped stores because it is a rule about
/// [`WatchProgress`] rather than about either backend. A third-party store
/// owes the same answers (`tests/contracts/`); it may ask this for them, or
/// derive them its own way.
pub struct Inspection<'a> {
    observations: &'a [WatchProgress],
    checkpoint: Option<&'a BlockRef>,
}

impl<'a> Inspection<'a> {
    /// `observations` is every observation of one address, whatever its asset,
    /// and `checkpoint` the line of the snapshot they were read in.
    #[must_use]
    pub const fn new(observations: &'a [WatchProgress], checkpoint: Option<&'a BlockRef>) -> Self {
        Self {
            observations,
            checkpoint,
        }
    }

    /// Whether any observation of the address is still reading its own past.
    ///
    /// [`WatchProgress::caught_up`] decides, which is the comparison a pass
    /// selects by: a store and an index cannot disagree about what "still
    /// catching up" means.
    ///
    /// **An observed address at a network with no line has read nothing.** A
    /// pass owes nothing below a line that does not exist, so `caught_up` says
    /// yes there; a read cannot, because no block has been committed for the
    /// network since it was created or purged ([`crate::IndexStore::purge`]). An empty answer
    /// from that snapshot is the absence of reading, not of rows.
    fn outstanding(&self) -> bool {
        let Some(line) = self.checkpoint else {
            return !self.observations.is_empty();
        };
        self.observations
            .iter()
            .any(|observed| !observed.caught_up(Some(line)))
    }

    /// Refuses a spendable set while the rows it would be read from are still
    /// arriving.
    ///
    /// **The one read that cannot be weakened instead.** A balance and a
    /// history page can say how far behind their rows are, so they answer
    /// honestly while an inspection is outstanding and the caller decides what
    /// a partial answer is worth. An [`crate::OutputPage`] carries no such
    /// claim and could not use one:
    /// its consumer is coin selection, which pages the set and spends the
    /// union, so an output offered becomes a transaction input. An unspent row
    /// at block 0 is only unspent *as far as has been read*, and until the
    /// inspection reaches the checkpoint the page is read at, the blocks
    /// between may hold the very spend that settles it. The rows exist, the
    /// checkpoint is real, and the two together are still a lie, which leaves
    /// refusing as the only honest answer.
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::Conflict`], the refusal every caller of
    /// [`crate::OutputStore`] already restarts on. It clears the moment the catch-up
    /// reaches the line, or, at a network with no line yet (never indexed, or
    /// [`crate::IndexStore::purge`] emptied it), at the first commit.
    pub fn spendable(&self) -> Result<(), IndexError> {
        if !self.outstanding() {
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
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{Address, AssetId, BlockHash, BlockHeight};

    fn observed(start: u64, through: Option<u64>) -> WatchProgress {
        let network = Network::new(ChainName::new("test"), "mainnet");
        WatchProgress {
            watch: Watch {
                id: "w".to_owned(),
                address: Address::new(network.clone(), "a"),
                asset: AssetId::native(network.chain.clone()),
                start: BlockPosition(start),
            },
            indexed_through: through.map(|position| BlockRef {
                position: BlockPosition(position),
                height: BlockHeight(position),
                hash: BlockHash(vec![1]),
                parent: None,
                timestamp: None,
            }),
        }
    }

    fn line(position: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(position),
            height: BlockHeight(position),
            hash: BlockHash(vec![2]),
            parent: None,
            timestamp: None,
        }
    }

    /// The `032` table, at the level of the type: a watch born at 20 and
    /// inspected through 25 resumes at 26, one born at 22 with nothing recorded
    /// resumes at 22, and one inspected through 30 resumes at 31.
    #[test]
    fn a_watch_resumes_after_what_was_inspected_for_it() {
        for (start, through, next) in [(20, Some(25), 26), (22, None, 22), (10, Some(30), 31)] {
            assert_eq!(
                observed(start, through).next_required(),
                Some(BlockPosition(next))
            );
        }
    }

    /// An exhausted native position owes no successor and cannot spin forever.
    #[test]
    fn an_exhausted_position_owes_nothing_and_never_spins() {
        let exhausted = observed(0, Some(u64::MAX));
        assert_eq!(exhausted.next_required(), None);
        assert!(exhausted.caught_up(Some(&line(u64::MAX))));
    }

    /// The predicate every selection and every store's claim asks: a watch owing a
    /// block at or below the line is behind, one whose next block is above it is
    /// current, and a network with no line at all is owed nothing.
    #[test]
    fn caught_up_is_whether_anything_below_the_line_is_still_owed() {
        assert!(!observed(10, None).caught_up(Some(&line(1500))));
        assert!(!observed(10, Some(1499)).caught_up(Some(&line(1500))));
        assert!(observed(10, Some(1500)).caught_up(Some(&line(1500))));
        assert!(observed(1501, None).caught_up(Some(&line(1500))));
        assert!(observed(10, None).caught_up(None));
    }
}
