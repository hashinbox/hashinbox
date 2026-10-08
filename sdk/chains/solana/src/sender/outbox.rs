//! [`Outbox`]: every native SOL send of one factory's wallets, in five
//! stages: reserve → prepare → sign → submit → settle.
//!
//! - **reserve** the senders (`SourceBusy` when one is taken),
//! - **prepare**: bracket every account at one slot, take a blockhash
//!   lifetime, price each message, and check the batch's funds,
//! - **sign** each message with its sender's signer and simulate it,
//! - **submit** through a registered [`SubmissionTask`]: re-check the
//!   lifetime, then send each exact envelope in order, up to three times,
//! - **settle**: accepted or definitely failed senders are released; an
//!   unknown outcome keeps its sender reserved until recovery proves the
//!   transaction present or absent.

use std::{collections::BTreeSet, sync::Arc};

use base::{Signer, TransactionError, transaction::Envelope};
use wallets::{SendError, SubmissionRegistrar, SubmissionTask, WalletError, WalletErrorKind};

use super::{
    dispatch::{Dispatch, Outcome},
    recovery::Settler,
    reservation::{Admission, PreparedTransaction, Reservations},
};
use crate::native::{
    self, Address, Lamport, Memo, SignedTransaction, TransactionId,
    rpc::{Accounts, Submitter},
    transaction::{Funds, Lifetime, Preflight, Transfer, UnsignedTransaction},
};

type MemoFactory = dyn Fn() -> Result<Memo, native::Error> + Send + Sync;

/// The send pipeline of one factory and its siblings: the only place that
/// reserves senders, signs, and submits.
pub(crate) struct Outbox {
    accounts: Arc<dyn Accounts>,
    preflight: Arc<dyn Preflight>,
    submitter: Arc<dyn Submitter>,
    registrar: Arc<dyn SubmissionRegistrar>,
    recovery: Arc<dyn Settler>,
    reservations: Reservations,
    memo: Arc<MemoFactory>,
}

/// One transfer, its sender, and the signer that controls the sender.
#[derive(Clone)]
pub(crate) struct SendRequest {
    from: Address,
    transfer: Transfer,
    signer: Arc<dyn Signer>,
    sweep: bool,
}

impl SendRequest {
    /// `transfer` from `from`, signed by `signer`.
    pub(crate) fn new(signer: Arc<dyn Signer>, from: Address, transfer: Transfer) -> Self {
        Self {
            from,
            transfer,
            signer,
            sweep: false,
        }
    }
}

impl SendRequest {
    /// A remainder request, resolved and validated before signing.
    pub(crate) fn sweep(
        signer: Arc<dyn Signer>,
        from: Address,
        to: Address,
    ) -> Result<Self, WalletError> {
        super::destination(&from, &to)?;
        Ok(Self {
            from,
            transfer: Transfer {
                to,
                amount: Lamport::ZERO,
            },
            signer,
            sweep: true,
        })
    }
}

/// Signed transactions in submission order, each with the floor simulation
/// left the whole batch at.
struct SignedBatch(Vec<PreparedTransaction>);

impl SignedBatch {
    /// `transactions`, the last of them simulated at the slot `floor_slot`.
    fn simulated(transactions: Vec<SignedTransaction>, floor_slot: u64) -> Self {
        let prepared = transactions
            .into_iter()
            .map(|transaction| PreparedTransaction {
                transaction,
                floor_slot,
            })
            .collect();
        Self(prepared)
    }
}

impl Outbox {
    pub(crate) fn new(
        accounts: Arc<dyn Accounts>,
        preflight: Arc<dyn Preflight>,
        submitter: Arc<dyn Submitter>,
        registrar: Arc<dyn SubmissionRegistrar>,
        recovery: Arc<dyn Settler>,
        reservations: Reservations,
    ) -> Self {
        Self {
            accounts,
            preflight,
            submitter,
            registrar,
            recovery,
            reservations,
            memo: Arc::new(Memo::generate),
        }
    }

    /// Sends `items` in order and returns every accepted transaction ID.
    ///
    /// # Errors
    ///
    /// Nothing is submitted when a stage before submit fails. From submit on,
    /// the error carries the accepted prefix, the failed item, and, when the
    /// outcome is unknown, that item's local transaction ID.
    pub(crate) async fn send(
        &self,
        items: Vec<SendRequest>,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        let admission = self.reserve(&items)?;
        let (prepared, _) = self.prepare(&items).await?;
        self.submit(prepared, admission).await
    }

    /// Reserves, prepares, and signs one transfer, then releases the sender
    /// and holds the transaction for [`Self::broadcast`].
    ///
    /// Returns the transaction and the fee the node priced its message at.
    ///
    /// # Errors
    ///
    /// As [`Self::send`], in the single-transaction contract's error.
    pub(crate) async fn sign(
        &self,
        item: SendRequest,
    ) -> Result<(SignedTransaction, Lamport), TransactionError> {
        self.sign_one(item)
            .await
            .map_err(crate::error::send_failure)
    }

    async fn sign_one(&self, item: SendRequest) -> Result<(SignedTransaction, Lamport), SendError> {
        let items = [item];
        let _admission = self.reserve(&items)?;
        let (SignedBatch(mut prepared), mut fees) = self.prepare(&items).await?;
        let prepared = prepared.remove(0);
        let transaction = prepared.transaction.clone();
        self.reservations.hold(prepared);
        Ok((transaction, fees.remove(0)))
    }

    /// Submits and settles a transaction [`Self::sign`] produced, once.
    ///
    /// # Errors
    ///
    /// A definite error when this outbox does not hold exactly `id` with the
    /// bytes `envelope`; otherwise as [`Self::send`], in the
    /// single-transaction contract's error.
    pub(crate) async fn broadcast(
        &self,
        id: &TransactionId,
        envelope: &Envelope,
    ) -> Result<base::TransactionId, TransactionError> {
        self.broadcast_held(id, envelope)
            .await
            .map_err(crate::error::send_failure)
    }

    async fn broadcast_held(
        &self,
        id: &TransactionId,
        envelope: &Envelope,
    ) -> Result<base::TransactionId, SendError> {
        let prepared = self.reservations.take(id, envelope).ok_or_else(|| {
            SendError::definite(
                WalletErrorKind::Transaction,
                "Solana transaction was not prepared by this wallet factory or was broadcast",
            )
        })?;
        let admission = self
            .reservations
            .admit(std::slice::from_ref(&prepared.transaction.from), false)?;
        let accepted = self.submit(SignedBatch(vec![prepared]), admission).await?;
        <[_; 1]>::try_from(accepted)
            .map(|[id]| id)
            .map_err(|_| unavailable("Solana submission returned an invalid result count"))
    }

    /// This outbox with `registrar` in place of its own, sharing everything else.
    pub(crate) fn with_registrar(&self, registrar: Arc<dyn SubmissionRegistrar>) -> Self {
        Self {
            accounts: self.accounts.clone(),
            preflight: self.preflight.clone(),
            submitter: self.submitter.clone(),
            registrar,
            recovery: self.recovery.clone(),
            reservations: self.reservations.clone(),
            memo: self.memo.clone(),
        }
    }

    fn reserve(&self, items: &[SendRequest]) -> Result<Admission, SendError> {
        let payers = items
            .iter()
            .map(|item| item.from.clone())
            .collect::<Vec<_>>();
        self.reservations.admit(&payers, items.len() > 1)
    }

    /// Prepare and sign: nothing leaves the process until every item passed.
    /// Returns the batch and each item's priced fee, in item order.
    async fn prepare(
        &self,
        items: &[SendRequest],
    ) -> Result<(SignedBatch, Vec<Lamport>), SendError> {
        let (min_slot, balances) = self.balances(items).await?;
        let lifetime = self
            .preflight
            .lifetime(min_slot)
            .await
            .map_err(|_| unavailable("Solana transaction preparation failed"))?;
        let mut resolved = items.to_vec();
        let mut messages = self.messages(&resolved, &lifetime.value)?;
        let (mut min_slot, fees) = self.fees(&messages, lifetime.slot).await?;
        Self::resolve(&mut resolved, &balances, &fees)?;
        if items.iter().any(|item| item.sweep) {
            for (item, (message, bytes, memo)) in resolved.iter().zip(&mut messages) {
                *message =
                    UnsignedTransaction::new(&item.from, &item.transfer, *memo, &lifetime.value);
                *bytes = message
                    .bytes()
                    .map_err(|_| unavailable("Solana sweep message encoding failed"))?;
            }
            let (floor, final_fees) = self.fees(&messages, min_slot).await?;
            min_slot = floor;
            if final_fees != fees {
                return Err(unavailable(
                    "Solana sweep fee changed while resolving its amount",
                ));
            }
        }
        let items = resolved.as_slice();
        let transactions = Self::sign_each(items, messages, min_slot, &lifetime.value).await?;
        let mut floor_slot = min_slot;
        for (index, transaction) in transactions.iter().enumerate() {
            floor_slot = self.simulate(index, transaction, floor_slot).await?;
        }
        Ok((SignedBatch::simulated(transactions, floor_slot), fees))
    }

    /// Resolves source-final sweeps without counting incoming batch transfers.
    fn resolve(
        items: &mut [SendRequest],
        balances: &[Lamport],
        fees: &[Lamport],
    ) -> Result<(), SendError> {
        let mut funds = Funds::default();
        for (index, item) in items.iter_mut().enumerate() {
            if item.sweep {
                let short = || {
                    item_error(
                        index,
                        WalletErrorKind::InvalidAmount,
                        "Solana sweep has no available funds after transfers and fees",
                    )
                };
                let amount = funds
                    .remaining(&item.from, balances[index], fees[index])
                    .ok_or_else(short)?;
                item.transfer =
                    Transfer::new(item.transfer.to.clone(), amount).map_err(|_| short())?;
            }
            funds
                .require(
                    &item.from,
                    balances[index],
                    item.transfer.amount,
                    fees[index],
                )
                .map_err(|error| scoped(index, error))?;
        }
        Ok(())
    }

    /// Each item's spendable balance, from one bracket of every account the
    /// batch touches, and the bracket's slot.
    async fn balances(&self, items: &[SendRequest]) -> Result<(u64, Vec<Lamport>), SendError> {
        let query = items
            .iter()
            .flat_map(|item| [item.from.clone(), item.transfer.to.clone()])
            .collect::<Vec<_>>();
        let bracket = self
            .accounts
            .bracket(&query)
            .await
            .map_err(|_| unavailable("Solana account acquisition failed"))?;
        let balances = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                bracket
                    .balance(&item.from, &item.transfer.to)
                    .map_err(|error| scoped(index, error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((bracket.min_slot, balances))
    }

    /// Every message's fee, each read at or after the previous read's slot,
    /// and the last slot.
    async fn fees(
        &self,
        messages: &[(UnsignedTransaction, Vec<u8>, Memo)],
        mut min_slot: u64,
    ) -> Result<(u64, Vec<Lamport>), SendError> {
        let mut fees = Vec::with_capacity(messages.len());
        for (_, bytes, _) in messages {
            let fee = self
                .preflight
                .fee(bytes, min_slot)
                .await
                .map_err(|_| unavailable("Solana transaction preparation failed"))?;
            min_slot = fee.slot;
            fees.push(fee.value);
        }
        Ok((min_slot, fees))
    }

    /// One distinct message per item, each with a fresh unique memo.
    fn messages(
        &self,
        items: &[SendRequest],
        lifetime: &Lifetime,
    ) -> Result<Vec<(UnsignedTransaction, Vec<u8>, Memo)>, SendError> {
        let mut memos = BTreeSet::new();
        let mut encoded = BTreeSet::new();
        let mut messages = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let memo = (self.memo)().map_err(|_| {
                item_error(
                    index,
                    WalletErrorKind::Generation,
                    "Solana Memo generation failed",
                )
            })?;
            if !memos.insert(memo) {
                return Err(item_transaction(index, "Solana Memo token must be unique"));
            }
            let message = UnsignedTransaction::new(&item.from, &item.transfer, memo, lifetime);
            let bytes = message
                .bytes()
                .map_err(|_| item_transaction(index, "Solana message encoding failed"))?;
            if !encoded.insert(bytes.clone()) {
                return Err(item_transaction(index, "Solana messages must be distinct"));
            }
            messages.push((message, bytes, memo));
        }
        Ok(messages)
    }

    async fn sign_each(
        items: &[SendRequest],
        messages: Vec<(UnsignedTransaction, Vec<u8>, Memo)>,
        min_slot: u64,
        lifetime: &Lifetime,
    ) -> Result<Vec<SignedTransaction>, SendError> {
        let mut ids = BTreeSet::new();
        let mut envelopes = BTreeSet::new();
        let mut transactions = Vec::with_capacity(items.len());
        for (index, (item, (message, _, _))) in items.iter().zip(messages).enumerate() {
            let signed = message
                .sign(&item.from, lifetime.clone(), min_slot, item.signer.as_ref())
                .await
                .map_err(|_| item_transaction(index, "Solana envelope signing failed"))?;
            if !ids.insert(signed.id) || !envelopes.insert(signed.envelope.as_bytes().to_vec()) {
                return Err(item_transaction(
                    index,
                    "Solana signed envelopes must be distinct",
                ));
            }
            transactions.push(signed);
        }
        Ok(transactions)
    }

    async fn simulate(
        &self,
        index: usize,
        transaction: &SignedTransaction,
        min_slot: u64,
    ) -> Result<u64, SendError> {
        match self
            .preflight
            .simulate(transaction.envelope.as_bytes(), min_slot)
            .await
        {
            Ok(slot) => Ok(slot),
            Err(error) if error.kind == native::ErrorKind::Rejected => Err(item_transaction(
                index,
                "Solana transaction simulation failed",
            )),
            Err(_) => Err(unavailable("Solana transaction simulation is unavailable")),
        }
    }

    /// Submit and settle: from here the work runs in a registered task, so
    /// it completes even if the caller goes away.
    async fn submit(
        &self,
        prepared: SignedBatch,
        admission: Admission,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        let SignedBatch(transactions) = prepared;
        let first = transactions.first().ok_or_else(|| {
            SendError::definite(
                WalletErrorKind::InvalidBatch,
                "Solana submission requires at least one envelope",
            )
        })?;
        let dispatch = Dispatch {
            submitter: self.submitter.clone(),
        };
        dispatch.live(0, first, Vec::new(), "registration").await?;
        let settlement = admission.settlement();
        let (result_send, result_wait) = tokio::sync::oneshot::channel();
        let recovery = self.recovery.clone();
        let (task, activation) = SubmissionTask::dormant(async move {
            match dispatch.run(transactions, settlement).await {
                Outcome::Complete(result) => {
                    let _ = result_send.send(result);
                }
                Outcome::Unknown { error, transaction } => {
                    let _ = result_send.send(Err(error));
                    recovery.settle(*transaction).await;
                }
            }
        });
        self.registrar
            .register(task)
            .await
            .map_err(|_| unavailable("Solana submission registration failed"))?;
        activation.start();
        result_wait
            .await
            .unwrap_or_else(|_| Err(unavailable("Solana submission task ended without a result")))
    }

}

/// A node or index failure that belongs to no item.
fn unavailable(message: &str) -> SendError {
    SendError::definite(WalletErrorKind::Unavailable, message)
}

fn item_error(index: usize, kind: WalletErrorKind, message: &str) -> SendError {
    SendError::item(index, Vec::new(), WalletError::new(kind, message))
}

fn item_transaction(index: usize, message: &str) -> SendError {
    item_error(index, WalletErrorKind::Transaction, message)
}

/// A native rule failure: node-side failures belong to no item, rule
/// failures name the item that broke the rule.
fn scoped(index: usize, error: native::Error) -> SendError {
    if error.kind == native::ErrorKind::RpcUnavailable {
        return unavailable(&error.message);
    }
    SendError::item(index, Vec::new(), error.into())
}
