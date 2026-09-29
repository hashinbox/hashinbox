//! The watch <-> block-commit handshake of one network: `NetworkAdmission`,
//! the `SyncPlan` a pass takes from it, and the two permits (`CommitPermit`,
//! `PublicationPermit`) that serialize a checkpoint commit against publishing
//! a new address. Crate-private; nothing here is part of the public API.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use futures_channel::oneshot;

use crate::{Address, AddressFilter, BlockPosition, BlockRef, IndexError, IndexErrorKind};

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
fn coverage(filters: &[AddressFilter]) -> Coverage {
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
/// [`crate::Watching::watch`] and the pass on the supervisor's task.
///
/// 1. Sync plans ([`NetworkAdmission::plan`]): it captures filters,
///    checkpoint, revision and selection together.
/// 2. Sync commits each block under a [`CommitPermit`] ([`SyncPlan::begin`]),
///    which rechecks both and holds publication off while storage writes.
/// 3. A watch takes a [`PublicationPermit`]
///    ([`NetworkAdmission::publication`]): it waits for an in-flight commit and
///    holds new commits off while the row is written.
/// 4. [`PublicationPermit::complete`] bumps the revision, so a plan captured
///    before the watch fails its next commit with a retryable `Conflict`.
///
/// One [`crate::Service`] owns one, for its own network. No mutex guard is
/// held across `.await`.
#[derive(Default)]
pub(crate) struct NetworkAdmission {
    state: Mutex<State>,
}

impl NetworkAdmission {
    /// An admission with no known checkpoint and revision zero.
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
    /// The selection ([`SyncPlan::selection`]) is derived here rather than
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
    ) -> Result<SyncPlan, IndexError>
    where
        F: FnOnce(Option<&BlockRef>) -> Result<Vec<AddressFilter>, IndexError>,
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
        Ok(SyncPlan {
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
                if let Some(persisted) = reload.take()
                    && (!state.initialized || state.recovery || state.persisted != persisted)
                {
                    state.persisted = persisted;
                    state.initialized = true;
                    state.recovery = false;
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

    fn begin(self: &Arc<Self>, plan: &SyncPlan) -> Result<CommitPermit, IndexError> {
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
pub(crate) struct SyncPlan {
    filters: Vec<AddressFilter>,
    checkpoint: Option<BlockRef>,
    /// The publication count this plan was captured at: what a commit is
    /// rechecked against, and nothing else ([`NetworkAdmission::begin`]).
    revision: u64,
    selection: u64,
    admission: Arc<NetworkAdmission>,
}

impl SyncPlan {
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
    /// before it commits ([`crate::overlay::Frontier::outdated`]).
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
    admission: Arc<NetworkAdmission>,
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
    admission: Arc<NetworkAdmission>,
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

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    use futures_executor::block_on;

    use super::*;
    use crate::{BlockHash, BlockHeight, BlockParent, ChainName, Network};

    fn block(position: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(position),
            height: BlockHeight(position),
            hash: BlockHash(vec![position as u8]),
            parent: position.checked_sub(1).map(|parent| BlockParent {
                position: BlockPosition(parent),
                hash: BlockHash(vec![parent as u8]),
            }),
            timestamp: None,
        }
    }

    fn commit_error(result: Result<CommitPermit, IndexError>, message: &str) -> IndexError {
        match result {
            Ok(_) => panic!("{message}"),
            Err(error) => error,
        }
    }

    /// A captured plan over a fresh admission: every plan has one now.
    fn plan(filters: Vec<AddressFilter>, checkpoint: Option<BlockRef>) -> SyncPlan {
        Arc::new(NetworkAdmission::new())
            .plan(checkpoint, |_| Ok(filters))
            .expect("a fresh admission captures a plan")
    }

    fn filter(value: &str, position: u64) -> AddressFilter {
        AddressFilter {
            address: Address {
                network: Network {
                    chain: ChainName::new("test"),
                    name: "testing".into(),
                },
                value: value.into(),
            },
            start_position: BlockPosition(position),
        }
    }

    #[test]
    fn notifying_drains_all_waiters_even_when_one_receiver_was_dropped() {
        let (first_send, first_receive) = oneshot::channel();
        let (cancelled_send, cancelled_receive) = oneshot::channel();
        let (last_send, last_receive) = oneshot::channel();
        drop(cancelled_receive);
        let mut state = State {
            waiters: vec![first_send, cancelled_send, last_send],
            revision: 42,
            ..State::default()
        };
        state.notify();
        assert!(state.waiters.is_empty());
        assert_eq!(block_on(first_receive), Ok(()));
        assert_eq!(block_on(last_receive), Ok(()));
        state.notify();
        assert_eq!(state.revision, 42);
    }

    #[test]
    fn empty_plan_has_no_active_addresses() {
        let plan = plan(Vec::new(), None);

        assert_eq!(plan.earliest_position(), None);
        assert!(plan.active_addresses(BlockPosition(0)).is_empty());
        assert!(plan.active_addresses(BlockPosition(u64::MAX)).is_empty());
    }

    #[test]
    fn earliest_position_uses_native_birthdays_without_inventing_a_parent() {
        for (starts, expected) in [
            (vec![950, 900], Some(BlockPosition(900))),
            (vec![0], Some(BlockPosition(0))),
            (vec![u64::MAX], Some(BlockPosition(u64::MAX))),
            (vec![900, 900, 950], Some(BlockPosition(900))),
        ] {
            let filters = starts
                .into_iter()
                .enumerate()
                .map(|(index, position)| filter(&format!("address-{index}"), position))
                .collect();
            let plan = plan(filters, Some(block(1)));
            assert_eq!(plan.earliest_position(), expected);
        }
    }

    #[test]
    fn earliest_position_is_the_lowest_captured_birthday_not_the_checkpoint() {
        let captured = plan(
            vec![filter("future", 100), filter("earlier", 20)],
            Some(block(10)),
        );
        assert_eq!(captured.earliest_position(), Some(BlockPosition(20)));
        assert_eq!(captured.checkpoint(), Some(&block(10)));
        assert_eq!(plan(Vec::new(), Some(block(10))).earliest_position(), None);
    }

    #[test]
    fn active_addresses_use_inclusive_native_birthdays_and_preserve_input_order() {
        let filters = vec![
            filter("future", 107),
            filter("birthday", 103),
            filter("earlier", 100),
            filter("skipped", 102),
        ];
        let checkpoint = BlockRef {
            height: BlockHeight(2),
            ..block(103)
        };
        let plan = plan(filters.clone(), Some(checkpoint));

        assert!(plan.active_addresses(BlockPosition(99)).is_empty());
        assert_eq!(
            plan.active_addresses(BlockPosition(103)),
            vec![
                filters[1].address.clone(),
                filters[2].address.clone(),
                filters[3].address.clone(),
            ]
        );
        assert_eq!(
            plan.active_addresses(BlockPosition(107)),
            filters
                .iter()
                .map(|filter| filter.address.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn checkpoint_movement_keeps_the_captured_address_selection() {
        let admission = Arc::new(NetworkAdmission::new());
        let mut filters = vec![filter("selected", 103), filter("future", 107)];
        let selected = filters[0].address.clone();
        let mut plan = admission
            .plan(Some(block(100)), |_| Ok(filters.clone()))
            .expect("captured plan");
        filters[0].start_position = BlockPosition(0);
        filters.push(filter("registered-later", 101));

        for position in [107, 100] {
            plan.advance(block(position));
            assert_eq!(plan.earliest_position(), Some(BlockPosition(103)));
            assert_eq!(plan.checkpoint(), Some(&block(position)));
            assert!(plan.active_addresses(BlockPosition(102)).is_empty());
            assert_eq!(
                plan.active_addresses(BlockPosition(103)),
                vec![selected.clone()]
            );
        }
    }

    #[test]
    fn commit_wins_and_publication_uses_the_committed_checkpoint() {
        let admission = Arc::new(NetworkAdmission::new());
        let plan = admission
            .plan(None, |_| Ok(Vec::new()))
            .expect("initial plan");
        let mut commit = plan.begin().expect("commit permit");
        commit.start();

        let mut publication = Box::pin(admission.publication(None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            publication.as_mut().poll(&mut context),
            Poll::Pending
        ));

        commit
            .complete(Some(block(7)))
            .expect("checkpoint publication");
        let publication = block_on(publication).expect("waiting publication");
        assert_eq!(publication.checkpoint(), Some(&block(7)));
    }

    #[test]
    fn publication_wins_and_invalidates_the_older_sync_plan() {
        let admission = Arc::new(NetworkAdmission::new());
        let plan = admission
            .plan(Some(block(7)), |_| Ok(Vec::new()))
            .expect("initial plan");
        let publication =
            block_on(admission.publication(Some(block(7)))).expect("publication permit");

        assert_eq!(
            commit_error(plan.begin(), "publication must block commit"),
            IndexError::new(
                IndexErrorKind::Conflict,
                "checkpoint admission is busy",
                true
            )
        );
        publication.complete().expect("publish filter revision");
        assert_eq!(
            commit_error(plan.begin(), "old revision must not commit"),
            IndexError::new(
                IndexErrorKind::Conflict,
                "checkpoint or address revision changed before commit",
                true,
            )
        );
    }

    #[test]
    fn dropped_started_commit_requires_checkpoint_reload() {
        let admission = Arc::new(NetworkAdmission::new());
        let stale = admission
            .plan(Some(block(7)), |_| Ok(Vec::new()))
            .expect("initial plan");
        let mut commit = stale.begin().expect("commit permit");
        commit.start();
        drop(commit);

        assert_eq!(
            commit_error(stale.begin(), "recovery must block stale plan"),
            IndexError::new(
                IndexErrorKind::Conflict,
                "checkpoint admission requires repository reload",
                true,
            )
        );
        let reloaded = admission
            .plan(Some(block(8)), |_| Ok(Vec::new()))
            .expect("repository reload repairs admission");
        reloaded
            .begin()
            .expect("reloaded plan can commit")
            .complete(Some(block(8)))
            .expect("complete reloaded plan");
    }

    #[test]
    fn idle_publication_reloads_after_a_cancelled_commit_even_for_an_empty_checkpoint() {
        for persisted in [Some(block(8)), None] {
            let admission = Arc::new(NetworkAdmission::new());
            let plan = admission
                .plan(Some(block(7)), |_| Ok(Vec::new()))
                .expect("initial plan");
            let mut commit = plan.begin().expect("commit permit");
            commit.start();
            drop(commit);

            let publication = block_on(admission.publication(persisted.clone()))
                .expect("publication reloads the repository checkpoint");
            assert_eq!(publication.checkpoint(), persisted.as_ref());
            let state = admission.lock().expect("admission state");
            assert!(state.initialized);
            assert!(!state.recovery);
            assert!(state.publication);
            assert_eq!(state.persisted, persisted);
        }
    }

    #[test]
    fn dropped_publication_releases_waiters_without_changing_revision() {
        let admission = Arc::new(NetworkAdmission::new());
        let first = block_on(admission.publication(None)).expect("first publication");
        let mut second = Box::pin(admission.publication(None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));

        drop(first);
        let second = block_on(second).expect("drop wakes next publication");
        second.complete().expect("second publication completes");
        admission
            .plan(None, |_| Ok(Vec::new()))
            .expect("admission remains usable");
    }

    #[test]
    fn exhausted_revision_releases_publication_and_wakes_waiters() {
        let admission = Arc::new(NetworkAdmission::new());
        admission.lock().unwrap().revision = u64::MAX;
        let first = block_on(admission.publication(Some(block(7)))).unwrap();
        let mut second = Box::pin(admission.publication(None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));

        assert_eq!(
            first.complete(),
            Err(IndexError::new(
                IndexErrorKind::Store,
                "address filter revision is exhausted",
                false,
            ))
        );
        assert_eq!(admission.lock().unwrap().revision, u64::MAX);
        let second = block_on(second).expect("failed completion releases the waiter");
        assert_eq!(second.checkpoint(), Some(&block(7)));
        drop(second);
        assert!(!admission.lock().unwrap().publication);
    }

    #[test]
    fn abandoned_publication_waiter_is_a_terminal_store_error() {
        let admission = Arc::new(NetworkAdmission::new());
        let first = block_on(admission.publication(None)).unwrap();
        let mut second = Box::pin(admission.publication(None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));
        admission.lock().unwrap().waiters.clear();

        let error = block_on(second).err().expect("abandoned waiter error");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::Store,
                "address admission waiter was abandoned",
                false,
            )
        );
        assert!(admission.lock().unwrap().publication);
        drop(first);
        assert!(!admission.lock().unwrap().publication);
    }

    #[test]
    fn poisoned_admission_rejects_plan_before_capturing_filters() {
        let admission = Arc::new(NetworkAdmission::new());
        let poisoned = admission.clone();
        assert!(
            std::thread::spawn(move || {
                let _state = poisoned.state.lock().unwrap();
                panic!("poison the owned admission fixture");
            })
            .join()
            .is_err()
        );

        let error = admission
            .plan(None, |_| {
                panic!("poisoned state must precede filter capture")
            })
            .err()
            .expect("poisoned admission error");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::Store,
                "address admission lock is poisoned",
                false,
            )
        );
    }

    /// The selection is new only when it *widened*, and that is the trade the
    /// number exists to make.
    ///
    /// A filter's start position is the next block its observation needs, so it
    /// rises with every block that commits. Calling the selection new on any
    /// change would re-fetch and re-interpret every block above the line on
    /// every pass; calling it new only when the *address set* changes would miss
    /// a second observation of an address already in the set, registered from
    /// further back, which does newly cover held blocks.
    #[test]
    fn a_selection_is_new_when_it_widens_and_not_when_it_narrows() {
        let admission = Arc::new(NetworkAdmission::new());
        let capture = |filters: Vec<AddressFilter>| {
            admission
                .plan(Some(block(7)), |_| Ok(filters))
                .expect("captured plan")
                .selection()
        };

        let empty = capture(Vec::new());
        let first = capture(vec![filter("watched", 10)]);
        assert_ne!(first, empty, "an address entering the selection widens it");
        assert_eq!(
            capture(vec![filter("watched", 20)]),
            first,
            "a start that advanced with the line covers no block it did not",
        );
        assert_eq!(
            capture(vec![]),
            first,
            "an address leaving covers no block it did not, and the facts it is \
             already in are the history a withdrawal leaves stored",
        );
        let lower = capture(vec![filter("watched", 5)]);
        assert_ne!(
            lower, first,
            "a second observation of a watched address, from further back, does \
             newly cover the blocks between",
        );
        assert_ne!(
            capture(vec![filter("watched", 5), filter("another", 99)]),
            lower,
            "and so does an address nothing watched before",
        );
    }

    #[test]
    fn plan_captures_filters_without_holding_the_admission_lock() {
        let admission = Arc::new(NetworkAdmission::new());
        let inspected = admission.clone();

        admission
            .plan(None, |_| {
                assert!(inspected.state.try_lock().is_ok());
                Ok(Vec::new())
            })
            .expect("unlocked filter capture");
    }
}
