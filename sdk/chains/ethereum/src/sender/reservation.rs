//! Process-local state of the [`super::Outbox`]: which senders are busy,
//! the nonce floor of each, and every signed transaction whose submission
//! is not settled.
//!
//! A send reserves its senders from the nonce read until each of its
//! transactions is settled. A second send from a busy sender waits; a
//! sender with an unknown outcome is recovered before its next send. A
//! signed single transaction is held for its broadcast without reserving
//! its sender. Nothing here survives a restart.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::sync::Notify;

use crate::native::{
    Address, Error, ErrorKind, SignedTransaction, TransactionId, transaction::Nonces,
};

/// Signed single transactions held for a later broadcast, oldest evicted first.
const HELD: usize = 64;

/// The senders one outbox has in flight, shared by every wallet it serves.
#[derive(Clone, Default)]
pub(crate) struct Reservations {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
}

/// One signed transaction and the sender nonce it uses.
#[derive(Clone, Debug)]
pub(crate) struct PreparedTransaction {
    pub(crate) from: Address,
    pub(crate) nonce: u64,
    pub(crate) transaction: SignedTransaction,
}

/// What [`Reservations::admit`] answers: the reservation itself, or the
/// recovery that has to happen before one can be made.
pub(crate) enum AdmissionOutcome {
    /// Every sender is reserved until the admission drops.
    Admitted(Admission),
    /// Batch item `index`'s sender has the unknown outcome `id`; settle it
    /// first, then admit again.
    Recover { id: TransactionId, index: usize },
}

/// Senders reserved for one send. Dropping it releases them and forgets
/// every transaction of the send that was never submitted.
pub(crate) struct Admission {
    reservations: Reservations,
    operation: u64,
}

/// One registered transaction being submitted. [`Settlement::accept`] raises
/// its sender's nonce floor, [`Settlement::reject`] frees the nonce, and
/// dropping it otherwise keeps the exact envelope as an unknown outcome.
pub(crate) struct Settlement {
    reservations: Reservations,
    id: TransactionId,
    transaction: SignedTransaction,
    unknown: bool,
    armed: bool,
}

#[derive(Default)]
struct State {
    next_operation: u64,
    accounts: BTreeMap<Address, Account>,
    records: BTreeMap<TransactionId, Record>,
    held: VecDeque<PreparedTransaction>,
}

/// One sender: the send holding it, and the lowest nonce it may still use.
#[derive(Default)]
struct Account {
    active: Option<u64>,
    floor: Option<u64>,
    records: BTreeSet<TransactionId>,
}

struct Record {
    prepared: PreparedTransaction,
    operation: u64,
    status: Status,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Signed by a live send and not yet submitted.
    Prepared,
    /// A submission may have reached the node; the envelope is kept.
    Unknown,
    /// Being submitted or recovered right now.
    Settling,
}

impl Reservations {
    /// Reserves every sender in `senders` (each with its first batch item),
    /// waiting while any is busy.
    ///
    /// # Errors
    ///
    /// The first item's index when the process ran out of operation IDs.
    pub(crate) async fn admit(
        &self,
        senders: &[(Address, usize)],
    ) -> Result<AdmissionOutcome, (usize, Error)> {
        loop {
            let mut notified = std::pin::pin!(self.changed.notified());
            notified.as_mut().enable();
            match self.try_admit(senders)? {
                Some(outcome) => return Ok(outcome),
                None => notified.await,
            }
        }
    }

    /// The lowest nonce `from` may use, once this process saw one accepted.
    pub(crate) fn floor(&self, from: &Address) -> Option<u64> {
        self.lock()
            .accounts
            .get(from)
            .and_then(|account| account.floor)
    }

    /// Keeps `batch` under `admission` until each is settled.
    ///
    /// # Errors
    ///
    /// A definite failure when a transaction is registered twice, its sender is
    /// not held by `admission`, or an accepted transaction already used its
    /// nonce.
    pub(crate) fn register(
        &self,
        admission: &Admission,
        batch: &[PreparedTransaction],
    ) -> Result<(), Error> {
        let mut state = self.lock();
        for prepared in batch {
            if state.records.contains_key(&prepared.transaction.id) {
                return Err(Error::rpc(
                    "Ethereum signed transaction is already reserved",
                    false,
                ));
            }
            let account = state.accounts.get(&prepared.from);
            if account.is_none_or(|account| account.active != Some(admission.operation)) {
                return Err(Error::rpc(
                    "Ethereum sender lost its atomic outbox admission",
                    false,
                ));
            }
            if account.and_then(|account| account.floor) > Some(prepared.nonce) {
                return Err(Error::rpc(
                    "Ethereum transaction nonce was already used by an accepted transaction",
                    false,
                ));
            }
        }
        for prepared in batch {
            let id = prepared.transaction.id.clone();
            let account = state.accounts.entry(prepared.from.clone()).or_default();
            account.records.insert(id.clone());
            let record = Record {
                prepared: prepared.clone(),
                operation: admission.operation,
                status: Status::Prepared,
            };
            state.records.insert(id, record);
        }
        Ok(())
    }

    /// Takes the registered transaction `id` for submission, waiting while
    /// another caller settles it. `expected`, when given, must be its exact
    /// envelope.
    ///
    /// # Errors
    ///
    /// A definite failure when `id` is not reserved here or differs from
    /// `expected`.
    pub(crate) async fn settlement(
        &self,
        id: &TransactionId,
        expected: Option<&SignedTransaction>,
    ) -> Result<Settlement, Error> {
        loop {
            let mut notified = std::pin::pin!(self.changed.notified());
            notified.as_mut().enable();
            match self.try_settlement(id, expected)? {
                Some(settlement) => return Ok(settlement),
                None => notified.await,
            }
        }
    }

    /// Holds a signed single transaction for its broadcast. This does not
    /// reserve the sender: a later send may take the same nonce, and then
    /// this broadcast fails definitely.
    pub(crate) fn hold(&self, prepared: PreparedTransaction) {
        let mut state = self.lock();
        if state.held.len() >= HELD {
            state.held.pop_front();
        }
        state.held.push_back(prepared);
    }

    /// The held transaction with exactly `transaction`'s bytes, removed from
    /// the hold.
    pub(crate) fn take(&self, transaction: &SignedTransaction) -> Option<PreparedTransaction> {
        let mut state = self.lock();
        let index = state
            .held
            .iter()
            .position(|held| &held.transaction == transaction)?;
        state.held.remove(index)
    }

    fn try_admit(
        &self,
        senders: &[(Address, usize)],
    ) -> Result<Option<AdmissionOutcome>, (usize, Error)> {
        let mut state = self.lock();
        for (from, index) in senders {
            let Some(account) = state.accounts.get(from) else {
                continue;
            };
            if account.active.is_some() {
                return Ok(None);
            }
            let pending = account
                .records
                .iter()
                .find_map(|id| state.records.get_key_value(id));
            match pending.map(|(id, record)| (id, record.status)) {
                Some((id, Status::Unknown)) => {
                    let (id, index) = (id.clone(), *index);
                    return Ok(Some(AdmissionOutcome::Recover { id, index }));
                }
                Some(_) => return Ok(None),
                None => {}
            }
        }
        let operation = state.next_operation;
        state.next_operation = operation.checked_add(1).ok_or_else(|| {
            let index = senders.first().map_or(0, |(_, index)| *index);
            let error = Error::new(
                ErrorKind::Other,
                "Ethereum outbox exhausted operation identifiers",
            );
            (index, error)
        })?;
        for (from, _) in senders {
            state.accounts.entry(from.clone()).or_default().active = Some(operation);
        }
        drop(state);
        Ok(Some(AdmissionOutcome::Admitted(Admission {
            reservations: self.clone(),
            operation,
        })))
    }

    fn try_settlement(
        &self,
        id: &TransactionId,
        expected: Option<&SignedTransaction>,
    ) -> Result<Option<Settlement>, Error> {
        let mut state = self.lock();
        let record = state.records.get_mut(id).ok_or_else(|| {
            Error::rpc(
                "signed Ethereum transaction has no process-local nonce reservation",
                false,
            )
        })?;
        if expected.is_some_and(|transaction| transaction != &record.prepared.transaction) {
            return Err(Error::rpc(
                "signed Ethereum transaction differs from its reserved exact envelope",
                false,
            ));
        }
        let unknown = match record.status {
            Status::Prepared => false,
            Status::Unknown => true,
            Status::Settling => return Ok(None),
        };
        record.status = Status::Settling;
        Ok(Some(Settlement {
            reservations: self.clone(),
            id: id.clone(),
            transaction: record.prepared.transaction.clone(),
            unknown,
            armed: true,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn remove(&mut self, id: &TransactionId) -> Option<Record> {
        let record = self.records.remove(id)?;
        if let Some(account) = self.accounts.get_mut(&record.prepared.from) {
            account.records.remove(id);
        }
        Some(record)
    }
}

impl Settlement {
    pub(crate) const fn id(&self) -> &TransactionId {
        &self.id
    }

    /// The exact envelope to submit.
    pub(crate) const fn transaction(&self) -> &SignedTransaction {
        &self.transaction
    }

    /// Whether an earlier submission of this envelope has an unknown outcome.
    pub(crate) const fn unknown(&self) -> bool {
        self.unknown
    }

    /// The node has the transaction: its nonce is spent for good.
    ///
    /// # Errors
    ///
    /// When the record vanished or the nonce was the last one.
    pub(crate) fn accept(mut self) -> Result<TransactionId, Error> {
        let mut state = self.reservations.lock();
        let record = state.remove(&self.id).ok_or_else(|| {
            Error::rpc(
                "Ethereum transaction disappeared during submission settlement",
                true,
            )
        })?;
        let next = Nonces::floor_after(record.prepared.nonce)?;
        let account = state.accounts.entry(record.prepared.from).or_default();
        account.floor = Some(account.floor.map_or(next, |floor| floor.max(next)));
        self.armed = false;
        drop(state);
        self.reservations.changed.notify_waiters();
        Ok(self.id.clone())
    }

    /// The node definitely refused the first submission: the nonce is free.
    pub(crate) fn reject(mut self) {
        self.reservations.lock().remove(&self.id);
        self.armed = false;
        self.reservations.changed.notify_waiters();
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = self.reservations.lock();
        let unsubmitted = state
            .records
            .iter()
            .filter(|(_, record)| {
                record.operation == self.operation && record.status == Status::Prepared
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in unsubmitted {
            state.remove(&id);
        }
        for account in state.accounts.values_mut() {
            if account.active == Some(self.operation) {
                account.active = None;
            }
        }
        drop(state);
        self.reservations.changed.notify_waiters();
    }
}

impl Drop for Settlement {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(record) = self.reservations.lock().records.get_mut(&self.id)
            && record.status == Status::Settling
        {
            record.status = Status::Unknown;
        }
        self.reservations.changed.notify_waiters();
    }
}
