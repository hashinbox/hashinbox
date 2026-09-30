//! The rows one network's watch set holds: one observation, and the block
//! through which it has been inspected.
//!
//! **One collection, keyed by watch identity.** An observation is what a caller
//! registers and withdraws, and how far it has been inspected is a fact about
//! that observation, so the two live in one row
//! ([`crate::WatchProgress`]) — which is also exactly what
//! [`crate::WatchStore::all`] hands back, so nothing here re-pairs anything. A
//! wallet holding a chain's own currency and a token at one address is two
//! observations with two rows and two claims, and they advance together because
//! one inspection of the address serves both
//! ([`crate::BlockInterpreter::inspect`] takes addresses and no asset), not
//! because they share a row.
//!
//! Inspection is still deduplicated by *address*, in [`WatchRows::filters`] and in
//! [`WatchRows::behind`]: a pass fetches and interprets a block once per address
//! however many assets are observed at it.

use std::collections::BTreeMap;

use crate::{Address, BlockPosition, BlockRef, Progress, WatchProgress};

/// Everything one network's watch set holds, under one lock.
#[derive(Default)]
pub(crate) struct WatchRows {
    /// Every observation and its progress, by the observation's own identity.
    pub(crate) rows: BTreeMap<String, WatchProgress>,
}

impl WatchRows {
    /// The selection a pass inspects the blocks above the confirmed line for,
    /// in address order, **deduplicated**.
    ///
    /// Only observations that are [`caught up`](WatchProgress::caught_up) with
    /// `checkpoint`: one still reading its own past is inspected for the blocks
    /// it owes and not for the ones arriving now, which is the trade that
    /// predicate names and the whole reason one reference can describe a watch's
    /// coverage.
    ///
    /// Each address enters once, from the lowest block any caught-up observation
    /// of it still needs — its [`next_required`](WatchProgress::next_required),
    /// not its birthday, because a caught-up observation resumes after what was
    /// already inspected for it.
    pub(crate) fn filters(&self, checkpoint: Option<&BlockRef>) -> Vec<crate::ForwardRange> {
        let mut earliest: BTreeMap<&Address, BlockPosition> = BTreeMap::new();
        for row in self.rows.values() {
            let Some(next) = row.next_required().filter(|_| row.caught_up(checkpoint)) else {
                continue;
            };
            earliest
                .entry(&row.watch.address)
                .and_modify(|start| *start = (*start).min(next))
                .or_insert(next);
        }
        earliest
            .into_iter()
            .map(|(address, start_position)| crate::ForwardRange {
                address: address.clone(),
                start_position,
            })
            .collect()
    }

    /// Every observation still reading its own past, with what it needs next.
    ///
    /// The complement of [`Self::filters`] under the same predicate, so no
    /// observation is in both and none is in neither: what one pass inspects at
    /// the front and what it fills in below the line are one partition of the
    /// set.
    pub(crate) fn behind(&self, checkpoint: Option<&BlockRef>) -> Vec<OwedRange> {
        self.rows
            .values()
            .filter(|row| !row.caught_up(checkpoint))
            .filter_map(|row| {
                Some(OwedRange {
                    watch_id: row.watch.id.clone(),
                    address: row.watch.address.clone(),
                    expected: row.indexed_through.clone(),
                    resume: row.next_required()?,
                })
            })
            .collect()
    }

    /// The claims one committed block proves, for the observations inspecting the
    /// front of the chain.
    ///
    /// `line` is the checkpoint as the pass holds it — the block this one
    /// extends — so the partition [`Self::filters`] captured is the partition
    /// advanced here, and an observation still reading its own past is untouched
    /// however far the line moves.
    ///
    /// A claim carries the value its author believed the store held, which is
    /// what the store compares ([`Progress`]).
    pub(crate) fn forward(&self, line: Option<&BlockRef>, block: &BlockRef) -> Vec<Progress> {
        self.rows
            .values()
            .filter(|row| row.caught_up(line))
            .filter_map(|row| {
                let next = row.next_required()?;
                (next <= block.position).then(|| Progress {
                    watch_id: row.watch.id.clone(),
                    expected: row.indexed_through.clone(),
                    indexed_through: block.clone(),
                })
            })
            .collect()
    }

    /// Takes over what a committed batch proved: the claims are durable now, so
    /// the rows they name hold an answer a restart will find.
    pub(crate) fn advanced(&mut self, proved: &[Progress]) {
        for claim in proved {
            if let Some(row) = self.rows.get_mut(&claim.watch_id) {
                row.indexed_through = Some(claim.indexed_through.clone());
            }
        }
    }

    /// Takes over everything the store holds for this network.
    ///
    /// Merge-only for the observations, so a watch registered while the read was
    /// in flight is not replaced by a row that predates it; **authoritative for
    /// the progress**, because the index only ever advances a row after the
    /// store confirmed the write, so the store's value is this one or newer. A
    /// batch whose outcome was lost is exactly the "newer" case, and reloading is
    /// how the index finds out.
    pub(crate) fn restore(&mut self, stored: Vec<WatchProgress>) {
        for row in stored {
            match self.rows.get_mut(&row.watch.id) {
                Some(held) => held.indexed_through = row.indexed_through,
                None => drop(self.rows.insert(row.watch.id.clone(), row)),
            }
        }
    }

    /// Every watch, in id order, as [`crate::Watching::watched`] returns them.
    pub(crate) fn watches(&self) -> Vec<crate::Watch> {
        self.rows.values().map(|row| row.watch.clone()).collect()
    }

    /// The watch `id` names, whatever its address.
    pub(crate) fn identified(&self, id: &str) -> Option<crate::Watch> {
        self.rows.get(id).map(|row| row.watch.clone())
    }

    /// The observation that already holds `(address, asset)`, if any.
    pub(crate) fn holder(&self, address: &Address, asset: &crate::AssetId) -> Option<crate::Watch> {
        self.rows
            .values()
            .map(|row| &row.watch)
            .find(|watch| &watch.address == address && &watch.asset == asset)
            .cloned()
    }

    /// Puts `watch` in the set with nothing inspected for it yet.
    ///
    /// Nothing is inherited and nothing is reset. A new observation owes its own
    /// range from its own birthday, and every other observation — including
    /// another one of the same address, registered earlier and possibly from
    /// further back — keeps the claim it earned. That is what per-observation
    /// progress buys over a claim shared by address, where a second observation
    /// from an earlier birthday had to widen the shared range and a restart in
    /// between could read the narrower row back as settled
    /// (`docs/design/030 §8 (ex-032)`, the first defect).
    pub(crate) fn attach(&mut self, watch: crate::Watch) {
        drop(
            self.rows
                .insert(watch.id.clone(), WatchProgress::unread(watch)),
        );
    }

    /// Forgets the observation `id` names, and the progress it earned with it.
    ///
    /// By identity, because that is all a row is keyed by and all a withdrawal
    /// ever knew: the same call settles a watch whose durable removal is
    /// confirmed and one whose removal is merely uncertain, where no `Watch`
    /// value may be at hand (`Reconciliation` in [`crate::watches`]).
    pub(crate) fn detach(&mut self, id: &str) {
        drop(self.rows.remove(id));
    }
}

/// The range one observation still owes: the address to inspect, and where to
/// resume.
///
/// The reduced form of a [`WatchProgress`] the filling half of a pass needs, and
/// it carries `expected` because the claim it will write has to say what it
/// believed the store held ([`Progress`]).
pub(crate) struct OwedRange {
    /// The observation's identity, which the claim is written against.
    pub(crate) watch_id: String,
    /// The address to inspect for it.
    pub(crate) address: Address,
    /// The progress this observation believes the store holds.
    pub(crate) expected: Option<BlockRef>,
    /// The first block still owed.
    pub(crate) resume: BlockPosition,
}
