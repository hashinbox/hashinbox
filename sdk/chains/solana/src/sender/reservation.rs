//! Process-local state of the [`super::outbox::Outbox`]: which senders are
//! busy, and which signed transactions wait for their broadcast.
//!
//! A sender is reserved from the first read until its transfers are settled.
//! A second send from a busy sender fails at once with `SourceBusy`; nothing
//! waits. Nothing here survives a restart.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use base::transaction::Envelope;
use wallets::{SendError, WalletError, WalletErrorKind};

use crate::native::{Address, SignedTransaction, TransactionId};

/// Signed transactions held for a later broadcast, the soonest to expire
/// evicted first.
///
/// Bitcoin's and ethereum's holds are queues and evict the oldest entry; this
/// one is keyed by transaction ID and evicts the entry whose blockhash
/// lifetime ends first, because a Solana transaction stops being broadcastable
/// at a known block height rather than merely getting older.
const HELD: usize = 64;

/// The senders one outbox has in flight, shared by every wallet it serves.
#[derive(Clone, Default)]
pub(crate) struct Reservations {
    state: Arc<Mutex<State>>,
}

/// Senders reserved while their transfers are prepared and signed. Dropping
/// it releases them.
pub(crate) struct Admission {
    reservations: Reservations,
    payers: Vec<Address>,
}

/// Senders whose signed transactions are being submitted. Dropping it
/// releases them.
pub(crate) struct Settlement {
    reservations: Reservations,
    payers: BTreeSet<Address>,
}

/// One signed transaction and the floor its batch's simulation reached — the
/// same role, under the same name, as bitcoin's and ethereum's
/// `PreparedTransaction`.
///
/// Two slots, two names: `transaction.min_slot` is the slot the transaction
/// was built at, and `floor_slot` is where simulation left the batch, at or
/// after it. Submission reads at the floor.
pub(crate) struct PreparedTransaction {
    pub(crate) transaction: SignedTransaction,
    pub(crate) floor_slot: u64,
}

#[derive(Default)]
struct State {
    preparing: BTreeSet<Address>,
    submitting: BTreeSet<Address>,
    held: BTreeMap<TransactionId, PreparedTransaction>,
}

impl Reservations {
    /// Reserves every sender in `payers` (one per item, in item order).
    ///
    /// # Errors
    ///
    /// `SourceBusy` when any sender is already reserved: for a batch it names
    /// the earliest item whose sender is busy.
    pub(crate) fn admit(&self, payers: &[Address], batch: bool) -> Result<Admission, SendError> {
        let mut state = self.lock();
        let busy = payers
            .iter()
            .position(|payer| state.preparing.contains(payer) || state.submitting.contains(payer));
        if let Some(index) = busy {
            let error = WalletError::new(
                WalletErrorKind::SourceBusy,
                "Solana source is already in use",
            );
            return Err(if batch {
                SendError::item(index, Vec::new(), error)
            } else {
                SendError::definite(WalletErrorKind::SourceBusy, error.message)
            });
        }
        let payers = payers.iter().cloned().collect::<BTreeSet<_>>();
        state.preparing.extend(payers.iter().cloned());
        drop(state);
        Ok(Admission {
            reservations: self.clone(),
            payers: payers.into_iter().collect(),
        })
    }

    /// Holds `prepared` for a later broadcast.
    pub(crate) fn hold(&self, prepared: PreparedTransaction) {
        let mut state = self.lock();
        if state.held.len() >= HELD {
            let expiring = state
                .held
                .iter()
                .min_by_key(|(_, held)| held.transaction.lifetime.last_valid_block_height())
                .map(|(id, _)| *id);
            if let Some(id) = expiring {
                state.held.remove(&id);
            }
        }
        state.held.insert(prepared.transaction.id, prepared);
    }

    /// The held transaction `id` with exactly the bytes `envelope`, and its
    /// simulated floor, removed from the hold.
    pub(crate) fn take(
        &self,
        id: &TransactionId,
        envelope: &Envelope,
    ) -> Option<PreparedTransaction> {
        let mut state = self.lock();
        let matches = state
            .held
            .get(id)
            .is_some_and(|held| &held.transaction.envelope == envelope);
        if !matches {
            return None;
        }
        state.held.remove(id)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Admission {
    /// Moves every reserved sender from preparation into submission at once.
    pub(crate) fn settlement(mut self) -> Settlement {
        let payers = std::mem::take(&mut self.payers)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut state = self.reservations.lock();
        for payer in &payers {
            state.preparing.remove(payer);
            state.submitting.insert(payer.clone());
        }
        drop(state);
        Settlement {
            reservations: self.reservations.clone(),
            payers,
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = self.reservations.lock();
        for payer in &self.payers {
            state.preparing.remove(payer);
        }
    }
}

impl Drop for Settlement {
    fn drop(&mut self) {
        let mut state = self.reservations.lock();
        for payer in &self.payers {
            state.submitting.remove(payer);
        }
    }
}
