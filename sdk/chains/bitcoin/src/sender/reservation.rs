//! Process-local state of the [`super::Outbox`]: which indexed outputs a
//! send has taken, and every signed transaction whose outcome keeps them.
//!
//! Selection and reservation happen under one lock, so two sends never
//! select the same output. An output stays reserved until the index no
//! longer lists it as unspent (its spend is indexed) or the send definitely
//! fails. An unknown outcome keeps its outputs until recovery settles it. A
//! signed single transaction is held for its broadcast without reserving
//! anything. Nothing here survives a restart.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use super::utxos::Utxos;
use crate::native::{
    Address, Error, SignedTransaction, TransactionId,
    block::{Outpoint, Utxo},
    transaction::UnsignedTransaction,
};

/// Signed single transactions held for a later broadcast, oldest evicted first.
const HELD: usize = 64;

/// The outputs one outbox has in flight, shared by every wallet it serves.
#[derive(Clone, Default)]
pub(crate) struct Reservations {
    state: Arc<Mutex<State>>,
}

/// The outputs one send selected. Dropping it before [`Admission::settlement`]
/// releases them.
pub(crate) struct Admission {
    reservations: Reservations,
    operation: u64,
}

/// A signed transaction being submitted. [`Settlement::accept`] keeps its
/// outputs reserved until their spend is indexed, [`Settlement::reject`] frees
/// them, and dropping it otherwise keeps them for an unknown outcome.
pub(crate) struct Settlement {
    reservations: Reservations,
    operation: u64,
    transaction: SignedTransaction,
    unknown: bool,
    armed: bool,
}

/// One signed transaction and the outputs it spends.
pub(crate) struct PreparedTransaction {
    inputs: Vec<(Outpoint, Address)>,
    transaction: SignedTransaction,
}

#[derive(Default)]
struct State {
    next_operation: u64,
    outpoints: BTreeMap<Outpoint, u64>,
    records: BTreeMap<u64, Record>,
    held: VecDeque<PreparedTransaction>,
}

struct Record {
    inputs: Vec<(Outpoint, Address)>,
    transaction: Option<SignedTransaction>,
    status: Status,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Selected and being signed; nothing was submitted.
    Preparing,
    /// Being submitted or recovered right now.
    Submitting,
    /// The node took it; the outputs wait for their spend to be indexed.
    Accepted,
    /// A submission may have reached the node; the envelope is kept.
    Unknown,
}

impl Reservations {
    /// Forgets reservations of `addresses` whose outputs `read` no longer
    /// lists as unspent, then lets `select` choose from the outputs no other
    /// send holds, and reserves its inputs.
    ///
    /// # Errors
    ///
    /// Whatever `select` returns, typically insufficient funds.
    pub(crate) fn admit(
        &self,
        addresses: &[Address],
        read: Utxos,
        select: impl FnOnce(Vec<Utxo>) -> Result<UnsignedTransaction, Error>,
    ) -> Result<(UnsignedTransaction, Admission), Error> {
        let mut state = self.lock();
        state.prune(addresses, &read);
        let available = read
            .utxos
            .into_iter()
            .filter(|utxo| !state.outpoints.contains_key(&utxo.outpoint))
            .collect();
        let unsigned = select(available)?;
        let inputs = unsigned
            .inputs
            .iter()
            .filter_map(|spend| {
                let address = spend.utxo.output.address.clone()?;
                Some((spend.utxo.outpoint, address))
            })
            .collect();
        let operation = state.insert(inputs, Status::Preparing);
        drop(state);
        let admission = Admission {
            reservations: self.clone(),
            operation,
        };
        Ok((unsigned, admission))
    }

    /// Every unknown outcome that spends an output of `addresses`.
    pub(crate) fn unknown(&self, addresses: &[Address]) -> Vec<SignedTransaction> {
        let state = self.lock();
        state
            .records
            .values()
            .filter(|record| record.status == Status::Unknown)
            .filter(|record| {
                let spends = |(_, owner): &(Outpoint, Address)| addresses.contains(owner);
                record.inputs.iter().any(spends)
            })
            .filter_map(|record| record.transaction.clone())
            .collect()
    }

    /// Takes the unknown outcome with exactly `transaction`'s bytes for
    /// recovery; `None` when there is none, or another caller settles it.
    pub(crate) fn settlement(&self, transaction: &SignedTransaction) -> Option<Settlement> {
        let mut state = self.lock();
        let (operation, record) = state.records.iter_mut().find(|(_, record)| {
            record.status == Status::Unknown && record.transaction.as_ref() == Some(transaction)
        })?;
        record.status = Status::Submitting;
        Some(Settlement {
            reservations: self.clone(),
            operation: *operation,
            transaction: transaction.clone(),
            unknown: true,
            armed: true,
        })
    }

    /// Releases `admission`'s outputs and holds `transaction`, which spends
    /// them, for its broadcast. A later send may select them first; then
    /// this broadcast fails definitely.
    pub(crate) fn hold(&self, admission: Admission, transaction: SignedTransaction) {
        let mut state = self.lock();
        let Some(record) = state.remove(admission.operation) else {
            return;
        };
        if state.held.len() >= HELD {
            state.held.pop_front();
        }
        state.held.push_back(PreparedTransaction {
            inputs: record.inputs,
            transaction,
        });
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

    /// Reserves the outputs a held transaction spends again.
    ///
    /// # Errors
    ///
    /// A definite failure when another send reserved one of them meanwhile.
    pub(crate) fn readmit(
        &self,
        held: PreparedTransaction,
    ) -> Result<(SignedTransaction, Admission), Error> {
        let mut state = self.lock();
        let taken = held
            .inputs
            .iter()
            .any(|(outpoint, _)| state.outpoints.contains_key(outpoint));
        if taken {
            return Err(Error::rpc(
                "Bitcoin outputs of this transaction were reserved by another send",
                false,
            ));
        }
        let operation = state.insert(held.inputs, Status::Preparing);
        drop(state);
        let admission = Admission {
            reservations: self.clone(),
            operation,
        };
        Ok((held.transaction, admission))
    }

    /// Every reserved output; for tests.
    #[cfg(test)]
    pub(crate) fn reserved(&self) -> Vec<Outpoint> {
        self.lock().outpoints.keys().copied().collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn insert(&mut self, inputs: Vec<(Outpoint, Address)>, status: Status) -> u64 {
        let operation = self.next_operation;
        self.next_operation = operation.wrapping_add(1);
        for (outpoint, _) in &inputs {
            self.outpoints.insert(*outpoint, operation);
        }
        let record = Record {
            inputs,
            transaction: None,
            status,
        };
        self.records.insert(operation, record);
        operation
    }

    fn remove(&mut self, operation: u64) -> Option<Record> {
        let record = self.records.remove(&operation)?;
        for (outpoint, _) in &record.inputs {
            if self.outpoints.get(outpoint) == Some(&operation) {
                self.outpoints.remove(outpoint);
            }
        }
        Some(record)
    }

    /// Drops the outputs of settled records that `read` shows spent, and
    /// every settled record left without outputs.
    fn prune(&mut self, addresses: &[Address], read: &Utxos) {
        let unspent = read
            .utxos
            .iter()
            .map(|utxo| utxo.outpoint)
            .collect::<BTreeSet<_>>();
        let mut spent = Vec::new();
        for record in self.records.values_mut() {
            if matches!(record.status, Status::Accepted | Status::Unknown) {
                spent.extend(record.spent(addresses, &unspent));
            }
        }
        for outpoint in spent {
            self.outpoints.remove(&outpoint);
        }
        self.records.retain(|_, record| !record.inputs.is_empty());
    }
}

impl Record {
    /// Forgets and returns the inputs of `addresses` missing from `unspent`.
    fn spent(&mut self, addresses: &[Address], unspent: &BTreeSet<Outpoint>) -> Vec<Outpoint> {
        let (spent, kept) =
            std::mem::take(&mut self.inputs)
                .into_iter()
                .partition(|(outpoint, owner)| {
                    addresses.contains(owner) && !unspent.contains(outpoint)
                });
        self.inputs = kept;
        spent.into_iter().map(|(outpoint, _)| outpoint).collect()
    }
}

impl Admission {
    /// Starts the submission of `transaction`, which spends exactly the
    /// reserved outputs.
    pub(crate) fn settlement(self, transaction: SignedTransaction) -> Settlement {
        if let Some(record) = self.reservations.lock().records.get_mut(&self.operation) {
            record.transaction = Some(transaction.clone());
            record.status = Status::Submitting;
        }
        Settlement {
            reservations: self.reservations.clone(),
            operation: self.operation,
            transaction,
            unknown: false,
            armed: true,
        }
    }
}

impl Settlement {
    /// The exact envelope to submit.
    pub(crate) const fn transaction(&self) -> &SignedTransaction {
        &self.transaction
    }

    /// Whether an earlier submission of this envelope has an unknown outcome.
    pub(crate) const fn unknown(&self) -> bool {
        self.unknown
    }

    /// The node has the transaction: its outputs stay reserved until the
    /// index shows them spent.
    pub(crate) fn accept(mut self) -> TransactionId {
        if let Some(record) = self.reservations.lock().records.get_mut(&self.operation) {
            record.status = Status::Accepted;
        }
        self.armed = false;
        self.transaction.id
    }

    /// The node definitely refused the transaction: its outputs are free.
    pub(crate) fn reject(mut self) {
        self.reservations.lock().remove(self.operation);
        self.armed = false;
    }
}

impl Drop for Admission {
    /// Releases the outputs unless the send reached submission.
    fn drop(&mut self) {
        let mut state = self.reservations.lock();
        let preparing = state
            .records
            .get(&self.operation)
            .is_some_and(|record| record.status == Status::Preparing);
        if preparing {
            state.remove(self.operation);
        }
    }
}

impl Drop for Settlement {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(record) = self.reservations.lock().records.get_mut(&self.operation) {
            record.status = Status::Unknown;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, time::Duration};

    use tokio::sync::Notify;
    use wallets::WalletErrorKind;

    use super::*;
    use crate::native::ErrorKind;
    use crate::sender::stub::{Action, Node};

    fn funded(outputs: &[u64]) -> Arc<Node> {
        let alice = Node::address(1);
        Arc::new(Node {
            outputs: Ok(outputs
                .iter()
                .map(|value| (alice.clone(), *value))
                .collect()),
            ..Node::default()
        })
    }

    fn inputs(transaction: &SignedTransaction) -> BTreeSet<bitcoin::OutPoint> {
        let native: bitcoin::Transaction =
            bitcoin::consensus::deserialize(transaction.envelope.as_bytes()).unwrap();
        native
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_sends_from_one_address_never_select_the_same_output() {
        let node = funded(&[100_000, 100_000]);
        let gate = Arc::new(Notify::new());
        node.actions
            .lock()
            .unwrap()
            .push_back(Action::Gate(gate.clone()));
        let outbox = Arc::new(node.outbox());
        let bob = Node::address(2);

        let first = tokio::spawn({
            let (outbox, bob) = (outbox.clone(), bob.clone());
            async move { outbox.send(vec![Node::item(1, &bob, 1_000)]).await }
        });
        while node.submitted.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        let second = outbox.send(vec![Node::item(1, &bob, 2_000)]).await;
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("the gated send must finish")
            .unwrap()
            .expect("the first send must be accepted");
        second.expect("the second send must take the other output");

        let submitted = node.submitted.lock().unwrap().clone();
        assert_eq!(submitted.len(), 2);
        let (a, b) = (inputs(&submitted[0]), inputs(&submitted[1]));
        assert_eq!((a.len(), b.len()), (1, 1));
        assert!(a.is_disjoint(&b), "two in-flight sends spent one output");
    }

    #[tokio::test]
    async fn an_accepted_transaction_keeps_its_outputs_until_the_index_shows_them_spent() {
        let node = funded(&[100_000]);
        let outbox = node.outbox();
        let bob = Node::address(2);
        outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect("the only output pays the first send");
        assert_eq!(outbox.reservations().reserved().len(), 1);

        let error = outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect_err("the accepted spend is not indexed yet");
        assert_eq!(error.source.kind, WalletErrorKind::Transaction);
        assert_eq!(
            error.source.message,
            "Bitcoin transfer has no available UTXOs"
        );
        assert_eq!(node.submitted.lock().unwrap().len(), 1);

        node.spent.lock().unwrap().insert(1);
        outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect_err("the index now shows the output spent");
        assert!(
            outbox.reservations().reserved().is_empty(),
            "an indexed spend releases its reservation"
        );
    }

    #[tokio::test]
    async fn a_definite_rejection_releases_the_outputs() {
        let node = funded(&[100_000]);
        node.actions
            .lock()
            .unwrap()
            .push_back(Action::Fail(Error::new(ErrorKind::Rejected, "code -26")));
        let outbox = node.outbox();
        let bob = Node::address(2);
        let error = outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect_err("the node refuses the first send");
        assert_eq!(error.ambiguous_transaction_id, None);
        assert!(outbox.reservations().reserved().is_empty());
        outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect("the released output pays the next send");
        let submitted = node.submitted.lock().unwrap().clone();
        assert_eq!(inputs(&submitted[0]), inputs(&submitted[1]));
    }

    #[tokio::test]
    async fn a_fresh_process_has_no_reservation() {
        let node = funded(&[100_000]);
        let bob = Node::address(2);
        let running = node.outbox();
        running
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .unwrap();
        assert!(
            running
                .send(vec![Node::item(1, &bob, 1_000)])
                .await
                .is_err()
        );

        node.outbox()
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect("a fresh process does not remember the in-flight spend");
    }

    /// `docs/design/030 §8 (ex-025)` section 5: selection folds the pending overlay in, so
    /// a block that reorgs out after its change was spent must not free what
    /// the accepted send holds. [`State::prune`] frees an outpoint only when
    /// a fresh read shows it **absent**; a reorg makes it reappear.
    #[tokio::test]
    async fn a_reorg_that_restores_an_output_keeps_its_reservation() {
        let node = funded(&[100_000]);
        let outbox = node.outbox();
        let bob = Node::address(2);
        outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect("the only output pays the first send");
        let reserved = outbox.reservations().reserved();
        assert_eq!(reserved.len(), 1);

        // The spend reaches a block, and that block then reorgs out: the
        // index lists the outpoint again before this wallet reads.
        node.spent.lock().unwrap().insert(1);
        node.spent.lock().unwrap().remove(&1);

        let error = outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect_err("the restored outpoint is still held by the first send");
        assert_eq!(
            error.source.message,
            "Bitcoin transfer has no available UTXOs"
        );
        assert_eq!(
            outbox.reservations().reserved(),
            reserved,
            "a restored outpoint keeps its reservation"
        );
        assert_eq!(
            node.submitted.lock().unwrap().len(),
            1,
            "the wallet never re-selected the output its own send spends"
        );
    }
}
