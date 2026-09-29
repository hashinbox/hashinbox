//! What the index observes: the durable list, and the selection one pass reads.

use crate::{Address, AssetId, BlockPosition, BoxFuture, Holder, IndexError, Network};

/// One observed address and the first block a pass must inspect for it.
///
/// The reduced form of a [`Watch`] that a pass actually needs, read from the
/// index's own watch set rather than handed in by a caller. `start_position` is
/// the address's *next required* block — the successor of whatever was last
/// inspected for it, or its birthday when nothing was — and not simply the
/// birthday, which is why this is derived from
/// [`crate::WatchProgress`] rather than from a [`Watch`] alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressFilter {
    /// The address to index, in its network's canonical text.
    pub address: Address,
    /// The first block this pass must inspect for the address: the successor of
    /// whatever was last inspected for it, or its birthday when nothing was.
    pub start_position: BlockPosition,
}

/// Where an address begins to be observed.
///
/// No default and no `Option`: a birthday below the checkpoint was once
/// accepted silently and indexed nothing (`docs/design/024` section 1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// This key or address has never been used, so observation begins at the
    /// block after the current checkpoint. An assertion of fact: a caller who
    /// does not know cannot honestly write it.
    Fresh,
    /// Observe from this native position.
    At(BlockPosition),
}

/// One observed address as the index holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    /// The caller's id, unique across the index.
    pub id: String,
    /// The address, and therefore the network.
    pub address: Address,
    /// The asset its history and balance are read in.
    pub asset: AssetId,
    /// The first block inspected for it.
    pub start: BlockPosition,
}

/// A stored watch is the holding it observes, under an id and a birthday, so
/// [`Holder::validate`] is the whole of what a store owes its address and asset
/// — the blank address and the foreign asset [`crate::WatchStore::add`]
/// refuses, checked by the row rather than by each store that holds one.
impl Holder for Watch {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &AssetId {
        &self.asset
    }
}

/// The index's durable list of what it observes.
///
/// A watch can be withdrawn. `029` section 5.2 refused an `unwatch` on two
/// grounds, and both are answered: the durable list's method budget was spent on
/// an `advance` that was never built — progress is written in the commit's own
/// transaction instead — and the fate of an unwatched address's committed
/// history is stated on [`Self::unwatch`] rather than left open.
/// Nothing is deleted, so the act is reversible, which is what makes it safe to
/// expose at all (`docs/design/030` G10).
pub trait Watching: Send + Sync {
    /// Observes `subject`'s address from `start`, durably, under `id`.
    ///
    /// The row is committed before this returns, so a caller that keeps a key
    /// for `id` keeps it *after* this call: a crash then leaves a watched
    /// address with no key, which is a watch-only address, rather than a key
    /// nothing observes (`docs/design/030 §8 (ex-029)` section 3.3).
    ///
    /// Re-watching the same address under the same id is a no-op; a known
    /// address under another id, another `Start::At`, or a known id on another
    /// address, is `Conflict`.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::NetworkMismatch`] for an address of another
    /// network, [`crate::IndexErrorKind::InvalidRequest`] for a blank address
    /// or an asset of another chain, [`crate::IndexErrorKind::Conflict`] for
    /// the collisions above, and the registry's own error.
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn Holder,
        start: Start,
    ) -> BoxFuture<'a, Result<Watch, IndexError>>;

    /// Stops observing whatever `id` watches, and returns the row it removed.
    ///
    /// **No indexed row is deleted.** The history, the live outputs and the
    /// credits of the address all stay; what stops is future inspection. Rows
    /// nothing claims are the price, and it is the cheaper one: deleting would be
    /// a second removal path beside the offline
    /// [`crate::Purging::purge_network`] and an irreversible one, for an act
    /// that is otherwise entirely reversible. Nor are the rows reachable
    /// meanwhile *through an application*: a wallet is addressed by `id`, so
    /// `GET /v1/wallets/{id}/balance` and the history page answer `404` the
    /// moment the row is gone, exactly as they do for an id that was never
    /// watched. An SDK caller holding the bare address still reads them, because
    /// [`crate::History`] and [`crate::Balances`] answer for a
    /// [`Holder`](crate::Holder) and consult no watch list — which is the same
    /// reason they answer for an address this process watched a moment ago.
    ///
    /// The one thing it does remove is the *claim*:
    /// [`crate::WatchProgress::indexed_through`]
    /// ([`crate::WatchStore::forget`]). It says every applicable block from this
    /// observation's birthday through that block was inspected for it, and it
    /// holds only while the observation exists — blocks committed while nothing
    /// watched the address were never inspected for it, and no field in the row
    /// tells the two apart. The claim is evidence about an observation, not about
    /// the chain, so it goes with the observation. Another observation of the same
    /// address keeps its own, earned under its own birthday.
    ///
    /// **The address may be watched again**, under any id and from any
    /// [`Start`], and it then reads its range again rather than inheriting a
    /// settled claim: bounded by the same
    /// [`SyncPolicy::max_backfill_blocks`](crate::SyncPolicy) the first watch was
    /// bounded by, and idempotent where the rows already exist (verified, not
    /// rewritten). Inspection *is* address-scoped and asset-independent by
    /// construction — [`crate::BlockInterpreter::inspect`] takes addresses and no
    /// asset — which is why one read of an address serves every asset observed at
    /// it (`docs/design/024` section 3.5). What is not inheritable is a claim
    /// across an interruption.
    ///
    /// **It orders against a running pass exactly as [`Self::watch`] does**,
    /// under the same publication permit and with no second mechanism: an
    /// in-flight commit finishes first, no block commits while the row is being
    /// removed, and the plan a pass captured before it fails its next commit
    /// with a retryable `Conflict` — so a chunk mid-inspection for this address
    /// is discarded rather than written after the row is gone.
    ///
    /// **Idempotent.** An `id` nothing watches is `Ok(None)`, not an error.
    ///
    /// # Errors
    ///
    /// The registry's own error, and [`crate::IndexErrorKind::Store`] when the
    /// watch set lock is poisoned.
    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>>;

    /// Every address this index observes on `network`, in id order.
    ///
    /// # Errors
    ///
    /// [`crate::IndexErrorKind::NetworkMismatch`] for a network this index
    /// does not own, and the registry's own error on the first read.
    fn watched<'a>(&'a self, network: &'a Network)
    -> BoxFuture<'a, Result<Vec<Watch>, IndexError>>;
}
