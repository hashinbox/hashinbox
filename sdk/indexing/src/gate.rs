//! **Action.**
//!
//! The watch <-> block-commit handshake of one network: `Gate`,
//! the `Plan` a pass takes from it, and the two permits (`CommitPermit`,
//! `PublicationPermit`) that serialize a checkpoint commit against publishing
//! a new address. Crate-private; nothing here is part of the public API.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use futures_channel::oneshot;

use crate::{Address, BlockPosition, BlockRef, ForwardRange, IndexError, IndexErrorKind};

#[derive(Default)]
struct State {
    initialized: bool,
    persisted: Option<BlockRef>,
    revision: u64,
    /// Which reading of the address selection the last captured plan was, and
    /// the coverage the last one described.
    selection: u64,
    coverage: Coverage,
    commit: bool,
    publication: bool,
    recovery: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

impl State {
    fn notify(&mut self) {
        for waiter in std::mem::take(&mut self.waiters) {
            let _ = waiter.send(());
        }
    }

    /// Hydrates publication from a store read without regressing a completed commit.
    fn observe(&mut self, persisted: Option<BlockRef>) -> Result<(), IndexError> {
        if !self.initialized || self.recovery {
            self.persisted = persisted;
            self.initialized = true;
            self.recovery = false;
            return Ok(());
        }
        let incoming = persisted.as_ref().map(|block| block.position);
        let current = self.persisted.as_ref().map(|block| block.position);
        if incoming == current && persisted != self.persisted {
            return Err(IndexError::retryable_conflict(
                "repository and admission disagree on the committed block",
            ));
        }
        if incoming > current {
            self.persisted = persisted;
        }
        Ok(())
    }

    /// The number naming the selection `coverage` describes, new only when that
    /// selection covers a block it did not cover before.
    ///
    /// **Directional, and that is the whole of why it costs nothing.** A
    /// filter's start position is the next block its observation needs, so every
    /// pass that commits anything raises it; a plain inequality here would call
    /// the selection new every single pass and re-fetch and re-interpret every
    /// block above the line for facts that cannot have changed. What a held
    /// block's facts can be *wrong* about is only ever an address that gained
    /// it — one that appeared in the selection, or one whose first covered block
    /// moved down — so a selection that only narrowed keeps its number and the
    /// held facts keep an address that stopped being watched, which is the same
    /// history a withdrawal leaves stored anyway
    /// ([`crate::WatchStore::forget`]).
    ///
    /// A count over a comparison rather than a hash of the coverage, because a
    /// collision here is a held block left standing at a selection it was never
    /// interpreted against — the one thing this number exists to catch.
    ///
    /// # Errors
    ///
    /// `Store` when the count overflows.
    fn select(&mut self, coverage: Coverage) -> Result<u64, IndexError> {
        let widened = coverage
            .iter()
            .any(|(address, from)| self.coverage.get(address).is_none_or(|held| from < held));
        self.coverage = coverage;
        if !widened {
            return Ok(self.selection);
        }
        self.selection = self.selection.checked_add(1).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Store,
                "address selection count is exhausted",
                false,
            )
        })?;
        Ok(self.selection)
    }
}

/// Which blocks above its checkpoint each address of one plan's selection is
/// inspected for: the address, and the first of them it covers.
///
/// Deliberately not the filter list itself, and deliberately compared in one
/// direction only ([`State::select`]).
type Coverage = BTreeMap<Address, BlockPosition>;

/// What `filters` covers, keyed by address as the selection already is.
fn coverage(filters: &[ForwardRange]) -> Coverage {
    filters
        .iter()
        .map(|filter| (filter.address.clone(), filter.start_position))
        .collect()
}

/// Serializes checkpoint commits with forward-only address publication.
///
/// This is the watch <-> block-commit handshake for one network. A new
/// address's birthday is the checkpoint's successor, so no block may commit
/// between reading that checkpoint and publishing the address's filter, and a
/// pass planned before the publication must not commit afterwards. The list
/// has one owner now, but two concurrent actors still reach it: a running
/// [`crate::Watching::watch`] and the pass on the driver's task.
///
/// 1. Sync plans ([`Gate::plan`]): it captures filters,
///    checkpoint, revision and selection together.
/// 2. Sync commits each block under a [`CommitPermit`] ([`Plan::begin`]),
///    which rechecks both and holds publication off while storage writes.
/// 3. A watch takes a [`PublicationPermit`]
///    ([`Gate::publication`]): it waits for an in-flight commit and
///    holds new commits off while the row is written.
/// 4. [`PublicationPermit::complete`] bumps the revision, so a plan captured
///    before the watch fails its next commit with a retryable `Conflict`.
///
/// One [`crate::Indexer`] owns one, for its own network. No mutex guard is
/// held across `.await`.
#[derive(Default)]
pub(crate) struct Gate {
    state: Mutex<State>,
}

impl Gate {
    /// A gate with no known checkpoint and revision zero.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Captures filters, their publication revision and their selection against
    /// one repository checkpoint.
    ///
    /// `capture` is handed that checkpoint, because which observations belong in
    /// the selection depends on it: one whose own history has not reached the
    /// line is inspected for what it owes and not for what is arriving
    /// ([`crate::WatchProgress::caught_up`]). It stays a *synchronous* closure
    /// holding no guard across an await, which is what makes the snapshot
    /// immutable.
    ///
    /// The selection ([`Plan::selection`]) is derived here rather than
    /// signalled by whatever changed the set, and that is the point: progress
    /// advancing, a store reload and a publication all move the same selection,
    /// so comparing the captured coverage catches every one of them without any
    /// of them having to remember to say so.
    ///
    /// # Errors
    ///
    /// A retryable `Conflict` when a commit or publication is in progress or
    /// the revision changed while `capture` ran; `capture`'s own error.
    pub(crate) fn plan<F>(
        self: &Arc<Self>,
        persisted: Option<BlockRef>,
        capture: F,
    ) -> Result<Plan, IndexError>
    where
        F: FnOnce(Option<&BlockRef>) -> Result<Vec<ForwardRange>, IndexError>,
    {
        let revision = {
            let mut state = self.lock()?;
            if state.commit || state.publication {
                return Err(IndexError::retryable_conflict(
                    "address admission is changing",
                ));
            }
            if !state.initialized || state.recovery || state.persisted != persisted {
                state.persisted = persisted.clone();
                state.initialized = true;
                state.recovery = false;
            }
            state.revision
        };
        let filters = capture(persisted.as_ref())?;
        let covered = coverage(&filters);
        let mut state = self.lock()?;
        if state.commit
            || state.publication
            || state.recovery
            || state.persisted != persisted
            || state.revision != revision
        {
            return Err(IndexError::retryable_conflict(
                "checkpoint or address revision changed during filter capture",
            ));
        }
        let selection = state.select(covered)?;
        Ok(Plan {
            filters,
            checkpoint: persisted,
            revision,
            selection,
            admission: self.clone(),
        })
    }

    /// Waits without holding the state lock, then reserves publication.
    ///
    /// `persisted` is the checkpoint the caller just read; the permit's
    /// [`PublicationPermit::checkpoint`] is the one to derive a birthday from.
    ///
    /// # Errors
    ///
    /// `Store` when the admission lock is poisoned or a waiter was abandoned.
    pub(crate) async fn publication(
        self: &Arc<Self>,
        persisted: Option<BlockRef>,
    ) -> Result<PublicationPermit, IndexError> {
        let mut reload = Some(persisted);
        loop {
            let wait = {
                let mut state = self.lock()?;
                let busy = state.commit || state.publication;
                if busy {
                    reload = None;
                }
                if let Some(persisted) = reload.take() {
                    state.observe(persisted)?;
                }
                if !busy {
                    state.publication = true;
                    return Ok(PublicationPermit {
                        admission: self.clone(),
                        checkpoint: state.persisted.clone(),
                        finished: false,
                    });
                }
                let (send, receive) = oneshot::channel();
                state.waiters.push(send);
                receive
            };
            wait.await.map_err(|_| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "address admission waiter was abandoned",
                    false,
                )
            })?;
        }
    }

    fn begin(self: &Arc<Self>, plan: &Plan) -> Result<CommitPermit, IndexError> {
        let mut state = self.lock()?;
        if state.recovery {
            return Err(IndexError::retryable_conflict(
                "checkpoint admission requires repository reload",
            ));
        }
        if state.commit || state.publication {
            return Err(IndexError::retryable_conflict(
                "checkpoint admission is busy",
            ));
        }
        if state.persisted != plan.checkpoint || state.revision != plan.revision {
            return Err(IndexError::retryable_conflict(
                "checkpoint or address revision changed before commit",
            ));
        }
        state.commit = true;
        Ok(CommitPermit {
            admission: self.clone(),
            started: false,
            finished: false,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, IndexError> {
        self.state.lock().map_err(|_| {
            IndexError::new(
                IndexErrorKind::Store,
                "address admission lock is poisoned",
                false,
            )
        })
    }
}

/// One immutable snapshot a sync plan works from: its filters, the checkpoint
/// they were captured against, the publication count a commit is rechecked
/// against, and the selection every held block is stamped with.
pub(crate) struct Plan {
    filters: Vec<ForwardRange>,
    checkpoint: Option<BlockRef>,
    /// The publication count this plan was captured at: what a commit is
    /// rechecked against, and nothing else ([`Gate::begin`]).
    revision: u64,
    selection: u64,
    admission: Arc<Gate>,
}

impl Plan {
    pub(crate) fn earliest_position(&self) -> Option<BlockPosition> {
        self.filters
            .iter()
            .map(|filter| filter.start_position)
            .min()
    }

    pub(crate) fn active_addresses(&self, position: BlockPosition) -> Vec<Address> {
        self.filters
            .iter()
            .filter(|filter| filter.start_position <= position)
            .map(|filter| filter.address.clone())
            .collect()
    }

    /// The checkpoint the plan was captured at, advanced as blocks commit.
    #[must_use]
    pub(crate) fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
    }

    /// The address selection this plan captured, as one number.
    ///
    /// Synchronization stamps it on every block it holds above the confirmed
    /// line, so a block interpreted against another selection is inspected again
    /// before it commits ([`crate::pending::Blocks::outdated`]).
    ///
    /// **Deliberately not [`Self::revision`].** That counts *publications*,
    /// which is what a commit is rechecked against; this names *which addresses
    /// cover which blocks above the line*, which is what those blocks' facts
    /// were produced from. Answering both with the publication count made
    /// admission the only thing that could invalidate held facts, so a watch
    /// that finished its own catch-up joined the forward selection while the
    /// blocks above the line stayed interpreted without it — no publication had
    /// happened, so the count had not moved — and they committed unchanged while
    /// its progress advanced through them.
    #[must_use]
    pub(crate) const fn selection(&self) -> u64 {
        self.selection
    }

    /// Reserves one repository transition.
    ///
    /// # Errors
    ///
    /// A retryable `Conflict` when a publication or commit is in progress,
    /// or the checkpoint or revision changed since the plan was captured.
    pub(crate) fn begin(&self) -> Result<CommitPermit, IndexError> {
        self.admission.clone().begin(self)
    }

    /// Records a committed block as the plan's checkpoint.
    pub(crate) fn advance(&mut self, checkpoint: BlockRef) {
        self.checkpoint = Some(checkpoint);
    }
}

/// Owns one asynchronous repository transition without holding a mutex guard.
///
/// Dropped without [`CommitPermit::complete`] after [`CommitPermit::start`],
/// the outcome is unknown, so the admission requires a repository reload.
pub(crate) struct CommitPermit {
    admission: Arc<Gate>,
    started: bool,
    finished: bool,
}

impl CommitPermit {
    /// Marks that storage I/O has begun.
    pub(crate) fn start(&mut self) {
        self.started = true;
    }

    /// Records the final checkpoint and reopens publication.
    ///
    /// # Errors
    ///
    /// `Store` when the admission lock is poisoned.
    pub(crate) fn complete(mut self, checkpoint: Option<BlockRef>) -> Result<(), IndexError> {
        let mut state = self.admission.lock()?;
        state.persisted = checkpoint;
        state.commit = false;
        state.notify();
        drop(state);
        self.finished = true;
        Ok(())
    }
}

impl Drop for CommitPermit {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut state) = self.admission.state.lock() {
            state.commit = false;
            state.recovery |= self.started;
            state.notify();
        }
    }
}

/// Holds publication closed against commits until the watch row is written.
pub(crate) struct PublicationPermit {
    admission: Arc<Gate>,
    checkpoint: Option<BlockRef>,
    finished: bool,
}

impl PublicationPermit {
    /// The checkpoint no block can move past while this permit is held.
    #[must_use]
    pub(crate) fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
    }

    /// Publishes: bumps the filter revision and reopens commits.
    ///
    /// # Errors
    ///
    /// `Store` when the admission lock is poisoned or the revision overflows.
    pub(crate) fn complete(mut self) -> Result<(), IndexError> {
        let mut state = self.admission.lock()?;
        state.revision = state.revision.checked_add(1).ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Store,
                "address filter revision is exhausted",
                false,
            )
        })?;
        state.publication = false;
        state.notify();
        drop(state);
        self.finished = true;
        Ok(())
    }
}

impl Drop for PublicationPermit {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut state) = self.admission.state.lock() {
            state.publication = false;
            state.notify();
        }
    }
}
