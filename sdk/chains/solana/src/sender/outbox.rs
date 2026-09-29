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
pub(crate) struct Item {
    from: Address,
    transfer: Transfer,
    signer: Arc<dyn Signer>,
}

impl Item {
    /// `transfer` from `from`, signed by `signer`.
    pub(crate) fn new(signer: Arc<dyn Signer>, from: Address, transfer: Transfer) -> Self {
        Self {
            from,
            transfer,
            signer,
        }
    }
}

/// What a signed batch needs for submission.
struct SignedBatch {
    min_slot: u64,
    transactions: Vec<SignedTransaction>,
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
        items: Vec<Item>,
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
        item: Item,
    ) -> Result<(SignedTransaction, Lamport), TransactionError> {
        self.sign_one(item)
            .await
            .map_err(crate::error::send_failure)
    }

    async fn sign_one(&self, item: Item) -> Result<(SignedTransaction, Lamport), SendError> {
        let items = [item];
        let _admission = self.reserve(&items)?;
        let (
            SignedBatch {
                min_slot,
                mut transactions,
            },
            mut fees,
        ) = self.prepare(&items).await?;
        let transaction = transactions.remove(0);
        self.reservations.hold(transaction.clone(), min_slot);
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
        let PreparedTransaction {
            transaction,
            min_slot,
        } = self.reservations.take(id, envelope).ok_or_else(|| {
            SendError::definite(
                WalletErrorKind::Transaction,
                "Solana transaction was not prepared by this wallet factory or was broadcast",
            )
        })?;
        let admission = self
            .reservations
            .admit(std::slice::from_ref(&transaction.from), false)?;
        let prepared = SignedBatch {
            min_slot,
            transactions: vec![transaction],
        };
        let accepted = self.submit(prepared, admission).await?;
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

    fn reserve(&self, items: &[Item]) -> Result<Admission, SendError> {
        let payers = items
            .iter()
            .map(|item| item.from.clone())
            .collect::<Vec<_>>();
        self.reservations.admit(&payers, items.len() > 1)
    }

    /// Prepare and sign: nothing leaves the process until every item passed.
    /// Returns the batch and each item's priced fee, in item order.
    async fn prepare(&self, items: &[Item]) -> Result<(SignedBatch, Vec<Lamport>), SendError> {
        let (min_slot, balances) = self.balances(items).await?;
        let lifetime = self
            .preflight
            .lifetime(min_slot)
            .await
            .map_err(|_| unavailable("Solana transaction preparation failed"))?;
        let messages = self.messages(items, &lifetime.value)?;
        let (min_slot, fees) = self.fees(&messages, lifetime.slot).await?;
        let mut funds = Funds::default();
        for (index, item) in items.iter().enumerate() {
            funds
                .require(
                    &item.from,
                    balances[index],
                    item.transfer.amount,
                    fees[index],
                )
                .map_err(|error| scoped(index, error))?;
        }
        let transactions = Self::sign_each(items, messages, min_slot, &lifetime.value).await?;
        let mut min_slot = min_slot;
        for (index, transaction) in transactions.iter().enumerate() {
            min_slot = self.simulate(index, transaction, min_slot).await?;
        }
        Ok((
            SignedBatch {
                min_slot,
                transactions,
            },
            fees,
        ))
    }

    /// Each item's spendable balance, from one bracket of every account the
    /// batch touches, and the bracket's slot.
    async fn balances(&self, items: &[Item]) -> Result<(u64, Vec<Lamport>), SendError> {
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
        messages: &[(UnsignedTransaction, Vec<u8>)],
        mut min_slot: u64,
    ) -> Result<(u64, Vec<Lamport>), SendError> {
        let mut fees = Vec::with_capacity(messages.len());
        for (_, bytes) in messages {
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
        items: &[Item],
        lifetime: &Lifetime,
    ) -> Result<Vec<(UnsignedTransaction, Vec<u8>)>, SendError> {
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
            messages.push((message, bytes));
        }
        Ok(messages)
    }

    async fn sign_each(
        items: &[Item],
        messages: Vec<(UnsignedTransaction, Vec<u8>)>,
        min_slot: u64,
        lifetime: &Lifetime,
    ) -> Result<Vec<SignedTransaction>, SendError> {
        let mut ids = BTreeSet::new();
        let mut envelopes = BTreeSet::new();
        let mut transactions = Vec::with_capacity(items.len());
        for (index, (item, (message, _))) in items.iter().zip(messages).enumerate() {
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
        let SignedBatch {
            min_slot,
            transactions,
        } = prepared;
        let first = transactions.first().ok_or_else(|| {
            SendError::definite(
                WalletErrorKind::InvalidBatch,
                "Solana submission requires at least one envelope",
            )
        })?;
        let dispatch = Dispatch {
            submitter: self.submitter.clone(),
            min_slot,
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

    /// An outbox that draws memos from `memos` instead of randomness.
    #[cfg(test)]
    pub(crate) fn with_memos(mut self, memos: impl IntoIterator<Item = Memo>) -> Self {
        let values =
            std::sync::Mutex::new(memos.into_iter().collect::<std::collections::VecDeque<_>>());
        self.memo = Arc::new(move || {
            values.lock().unwrap().pop_front().ok_or_else(|| {
                native::Error::new(native::ErrorKind::KeyGeneration, "fixture Memo exhausted")
            })
        });
        self
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

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use json_rpc::{Batch, BoxFuture, CallResult, RawJson, Request};
    use serde_json::{Value, json};
    use solana_signature::Signature;
    use tokio::sync::Notify;

    use super::*;
    use crate::native::{
        KeyPair,
        rpc::{
            Client,
            script::{Script, Scripted},
        },
    };
    use crate::sender::stub::{
        Node, Registrar, Resolution, key, lifetime, outbox as build, signed,
    };

    type Exchange = (&'static str, Value, Value);

    fn outbox(script: &Script, registrar: Arc<dyn SubmissionRegistrar>) -> Outbox {
        build(script.client(), registrar, Arc::new(Resolution::default()))
    }

    fn item(key: &Arc<KeyPair>, to: &Address, lamports: u64) -> Item {
        Item {
            from: key.address().clone(),
            transfer: Transfer {
                to: to.clone(),
                amount: Lamport::from_atomic(lamports),
            },
            signer: key.clone(),
        }
    }

    fn account(lamports: u64, owner: &Address) -> Value {
        json!({
            "lamports": lamports, "owner": owner.to_string(),
            "executable": false, "data": ["", "base64"], "space": 0,
        })
    }

    fn wallet(lamports: u64) -> Value {
        account(lamports, &Address::from_bytes([0; 32]))
    }

    /// Health, slot 5, the accounts of `query` at slot 5, closing slot 6, and
    /// a lifetime at slot 7.
    fn bracketed(query: &[&Address], values: Value) -> Vec<Exchange> {
        let texts = query.iter().map(ToString::to_string).collect::<Vec<_>>();
        vec![
            ("getHealth", json!([]), json!("ok")),
            ("getSlot", json!([{"commitment":"confirmed"}]), json!(5)),
            (
                "getMultipleAccounts",
                json!([texts, {"encoding":"base64", "commitment":"confirmed", "minContextSlot":5}]),
                json!({"context":{"slot":5},"value":values}),
            ),
            (
                "getSlot",
                json!([{"commitment":"confirmed","minContextSlot":5}]),
                json!(6),
            ),
            (
                "getLatestBlockhash",
                json!([{"commitment":"confirmed","minContextSlot":6}]),
                json!({"context":{"slot":7},"value":{
                    "blockhash": lifetime().blockhash().to_string(), "lastValidBlockHeight": 44,
                }}),
            ),
        ]
    }

    fn fee(message: &[u8], min_slot: u64, slot: u64) -> Exchange {
        (
            "getFeeForMessage",
            json!([STANDARD.encode(message), {"commitment":"confirmed","minContextSlot":min_slot}]),
            json!({"context":{"slot":slot},"value":5}),
        )
    }

    fn simulation(transaction: &SignedTransaction, min_slot: u64, result: Value) -> Exchange {
        (
            "simulateTransaction",
            json!([STANDARD.encode(transaction.envelope.as_bytes()), {
                "encoding": "base64", "commitment": "confirmed", "sigVerify": true,
                "replaceRecentBlockhash": false, "minContextSlot": min_slot,
            }]),
            result,
        )
    }

    fn height(value: Value) -> Exchange {
        (
            "getBlockHeight",
            json!([{"commitment":"confirmed", "minContextSlot":11}]),
            value,
        )
    }

    fn send(transaction: &SignedTransaction, returned: Value) -> Exchange {
        (
            "sendTransaction",
            json!([STANDARD.encode(transaction.envelope.as_bytes()), {
                "encoding": "base64", "skipPreflight": false, "preflightCommitment": "confirmed",
                "minContextSlot": 11, "maxRetries": 0,
            }]),
            returned,
        )
    }

    fn status(transaction: &SignedTransaction, value: Value) -> Exchange {
        (
            "getSignatureStatuses",
            json!([[transaction.id.to_string()], {"searchTransactionHistory":true}]),
            value,
        )
    }

    fn message(key: &KeyPair, to: &Address, memo: u8) -> UnsignedTransaction {
        let transfer = Transfer {
            to: to.clone(),
            amount: Lamport::from_atomic(10),
        };
        UnsignedTransaction::new(
            key.address(),
            &transfer,
            Memo::from_bytes([memo; Memo::LENGTH]),
            &lifetime(),
        )
    }

    fn busy(outbox: &Outbox, source: &Address) -> bool {
        outbox
            .reservations
            .admit(std::slice::from_ref(source), false)
            .is_err()
    }

    fn prepared(outbox: &Outbox, transactions: Vec<SignedTransaction>) -> (SignedBatch, Admission) {
        let admission = outbox
            .reservations
            .admit(std::slice::from_ref(&transactions[0].from), false)
            .expect("reservation");
        (
            SignedBatch {
                min_slot: 11,
                transactions,
            },
            admission,
        )
    }

    fn assert_definite(
        error: &SendError,
        index: Option<usize>,
        kind: WalletErrorKind,
        message: &str,
    ) {
        assert_eq!(error.failed_index, index);
        assert_eq!(
            (error.source.kind, error.source.message.as_str()),
            (kind, message)
        );
        assert!(error.accepted.is_empty());
        assert_eq!(error.ambiguous_transaction_id, None);
        assert_eq!(error.source.ambiguous_transaction_id, None);
    }

    #[tokio::test]
    async fn node_failures_are_operation_wide_and_redacted() {
        let signer = key(7);
        let to = key(8).address().clone();
        let bracket_failure = Script::exchanges([("getHealth", json!([]), json!("behind"))]);
        let error = outbox(&bracket_failure, Registrar::closed())
            .send(vec![item(&signer, &to, 10)])
            .await
            .unwrap_err();
        assert_definite(
            &error,
            None,
            WalletErrorKind::Unavailable,
            "Solana account acquisition failed",
        );
        bracket_failure.exhausted();

        let mut exchanges = bracketed(&[signer.address(), &to], json!([wallet(100), null]));
        exchanges.pop();
        exchanges.push((
            "getLatestBlockhash",
            json!([{"commitment":"confirmed","minContextSlot":6}]),
            json!({"context":{"slot":5},"value":{"blockhash":"bad","lastValidBlockHeight":1}}),
        ));
        let lifetime_failure = Script::exchanges(exchanges);
        let error = outbox(&lifetime_failure, Registrar::closed())
            .send(vec![item(&signer, &to, 10)])
            .await
            .unwrap_err();
        assert_definite(
            &error,
            None,
            WalletErrorKind::Unavailable,
            "Solana transaction preparation failed",
        );
        lifetime_failure.exhausted();
    }

    #[tokio::test]
    async fn semantic_failure_names_the_earliest_item_and_releases_every_sender() {
        let signer = key(7);
        let (to, other) = (key(8).address().clone(), key(10).address().clone());
        let script = Script::exchanges(
            bracketed(
                &[signer.address(), &to, &other],
                json!([account(1, key(9).address()), null, null]),
            )
            .into_iter()
            .take(4),
        );
        let outbox = outbox(&script, Registrar::closed());
        let error = outbox
            .send(vec![item(&signer, &to, 10), item(&signer, &other, 10)])
            .await
            .unwrap_err();
        assert_definite(
            &error,
            Some(0),
            WalletErrorKind::Unsupported,
            "unsupported Solana source account",
        );
        assert!(!busy(&outbox, signer.address()));
        script.exhausted();
    }

    #[tokio::test]
    async fn prepares_one_complete_envelope_in_strict_stage_order() {
        let signer = key(7);
        let to = key(8).address().clone();
        let unsigned = message(&signer, &to, 3);
        let bytes = unsigned.bytes().unwrap();
        let expected = unsigned
            .sign(signer.address(), lifetime(), 8, signer.as_ref())
            .await
            .unwrap();
        let mut exchanges = bracketed(&[signer.address(), &to], json!([wallet(100), null]));
        exchanges.push(fee(&bytes, 7, 8));
        exchanges.push(simulation(
            &expected,
            8,
            json!({"context":{"slot":9},"value":{"err":null}}),
        ));
        let script = Script::exchanges(exchanges);
        let (prepared, fees) = outbox(&script, Registrar::closed())
            .with_memos([Memo::from_bytes([3; Memo::LENGTH])])
            .prepare(&[item(&signer, &to, 10)])
            .await
            .expect("complete preparation");
        assert_eq!(prepared.min_slot, 9);
        assert_eq!(prepared.transactions, [expected]);
        assert_eq!(fees, [Lamport::from_atomic(5)]);
        script.exhausted();
    }

    #[tokio::test]
    async fn finishes_all_message_validation_before_any_fee_or_signature() {
        let signer = key(7);
        let to = key(8).address().clone();
        let memo = Memo::from_bytes([3; Memo::LENGTH]);
        for (memos, kind, message) in [
            (
                vec![memo],
                WalletErrorKind::Generation,
                "Solana Memo generation failed",
            ),
            (
                vec![memo, memo],
                WalletErrorKind::Transaction,
                "Solana Memo token must be unique",
            ),
        ] {
            let script = Script::exchanges(bracketed(
                &[signer.address(), &to],
                json!([wallet(100), null]),
            ));
            let error = outbox(&script, Registrar::closed())
                .with_memos(memos)
                .send(vec![item(&signer, &to, 10), item(&signer, &to, 10)])
                .await
                .unwrap_err();
            assert_definite(&error, Some(1), kind, message);
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn all_fees_precede_cumulative_funds_and_all_signatures_precede_simulation() {
        let first = key(7);
        let to = key(8).address().clone();
        for (second, signer, balance, expected) in [
            (key(7), key(7), 29, "Solana source balance is insufficient"),
            (key(9), key(7), 100, "Solana envelope signing failed"),
        ] {
            let mut query = vec![first.address(), &to];
            let mut values = vec![wallet(balance), Value::Null];
            if second.address() != first.address() {
                query.push(second.address());
                values.push(wallet(balance));
            }
            let mut exchanges = bracketed(&query, json!(values));
            exchanges.push(fee(&message(&first, &to, 3).bytes().unwrap(), 7, 8));
            exchanges.push(fee(&message(&second, &to, 4).bytes().unwrap(), 8, 9));
            let script = Script::exchanges(exchanges);
            let mut last = item(&second, &to, 10);
            last.signer = signer;
            let error = outbox(&script, Registrar::closed())
                .with_memos([3, 4].map(|byte| Memo::from_bytes([byte; Memo::LENGTH])))
                .send(vec![item(&first, &to, 10), last])
                .await
                .unwrap_err();
            assert_definite(&error, Some(1), WalletErrorKind::Transaction, expected);
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn simulation_keeps_context_precedence_and_the_items_index() {
        let transaction = signed(&key(7), 3, 8);
        for (response, index, kind, message) in [
            (
                json!({"context":{"slot":10},"value":{"err":"provider detail"}}),
                Some(7),
                WalletErrorKind::Transaction,
                "Solana transaction simulation failed",
            ),
            (
                json!({"context":{"slot":9},"value":{"err":"provider detail"}}),
                None,
                WalletErrorKind::Unavailable,
                "Solana transaction simulation is unavailable",
            ),
            (
                json!({"context":{"slot":10},"value":"malformed"}),
                None,
                WalletErrorKind::Unavailable,
                "Solana transaction simulation is unavailable",
            ),
        ] {
            let script = Script::exchanges([simulation(&transaction, 10, response)]);
            let error = outbox(&script, Registrar::closed())
                .simulate(7, &transaction, 10)
                .await
                .unwrap_err();
            assert_definite(&error, index, kind, message);
            script.exhausted();
        }
    }

    /// A node that answers the bracket reads, but never returns from call
    /// `block_at`.
    #[derive(Clone)]
    struct Blocking {
        block_at: usize,
        calls: Arc<AtomicUsize>,
        entered: Arc<Notify>,
    }

    impl json_rpc::Client for Blocking {
        fn request<'a>(
            &'a self,
            request: Request,
        ) -> BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
            let method = request.into_call().method;
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == self.block_at {
                    self.entered.notify_one();
                    std::future::pending::<()>().await;
                }
                let expected = ["getHealth", "getSlot", "getMultipleAccounts", "getSlot"];
                assert_eq!(method, expected[call], "downstream call after cancellation");
                let value = match call {
                    0 => json!("ok"),
                    2 => json!({"context":{"slot":1},"value":[wallet(10), null]}),
                    _ => json!(1),
                };
                Ok(Ok(RawJson::from_serializable(&value).unwrap()))
            })
        }

        fn batch<'a>(
            &'a self,
            _batch: Batch,
        ) -> BoxFuture<'a, Result<Vec<CallResult>, json_rpc::Error>> {
            unreachable!("account acquisition never batches")
        }
    }

    #[tokio::test]
    async fn dropping_a_send_at_any_rpc_await_stops_downstream_work_and_releases_the_sender() {
        for block_at in 0..4 {
            let signer = key(7);
            let to = key(8).address().clone();
            let calls = Arc::new(AtomicUsize::new(0));
            let entered = Arc::new(Notify::new());
            let transport = Blocking {
                block_at,
                calls: calls.clone(),
                entered: entered.clone(),
            };
            let outbox = Arc::new(build(
                Client::unverified(transport, Script::network()),
                Registrar::closed(),
                Arc::new(Resolution::default()),
            ));
            let sending = outbox.clone();
            let items = vec![item(&signer, &to, 1)];
            let task = tokio::spawn(async move { sending.send(items).await });
            entered.notified().await;
            assert!(busy(&outbox, signer.address()));
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(calls.load(Ordering::SeqCst), block_at + 1);
            assert!(!busy(&outbox, signer.address()));
        }
    }

    #[tokio::test]
    async fn an_empty_submission_is_invalid_and_releases_its_reservation() {
        let script = Script::exchanges([]);
        let outbox = outbox(&script, Registrar::closed());
        let (mut empty, admission) = prepared(&outbox, vec![signed(&key(7), 3, 11)]);
        empty.transactions.clear();
        let error = outbox.submit(empty, admission).await.unwrap_err();
        assert_definite(
            &error,
            None,
            WalletErrorKind::InvalidBatch,
            "Solana submission requires at least one envelope",
        );
        assert!(!busy(&outbox, key(7).address()));
        script.exhausted();
    }

    #[tokio::test]
    async fn the_first_lifetime_check_precedes_registration_and_keeps_its_network() {
        for (value, index, message) in [
            (
                json!(null),
                Some(0),
                "Solana block height is unavailable before registration",
            ),
            (
                json!(45),
                Some(0),
                "Solana transaction lifetime expired before registration",
            ),
            (json!(44), None, "Solana submission registration failed"),
        ] {
            let script = Script::exchanges([height(value)]);
            let outbox = outbox(&script, Registrar::closed());
            let (prepared, admission) = prepared(&outbox, vec![signed(&key(7), 3, 11)]);
            let error = outbox.submit(prepared, admission).await.unwrap_err();
            assert_definite(&error, index, WalletErrorKind::Unavailable, message);
            assert!(!busy(&outbox, key(7).address()));
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn dropping_the_send_before_acknowledgement_never_activates_and_releases_the_sender() {
        let script = Script::exchanges([height(json!(44))]);
        let outbox = outbox(&script, Registrar::withholding());
        let (prepared, admission) = prepared(&outbox, vec![signed(&key(7), 3, 11)]);
        let mut waiting = Box::pin(outbox.submit(prepared, admission));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Pending));
        assert!(busy(&outbox, key(7).address()));
        drop(waiting);
        assert!(!busy(&outbox, key(7).address()));
        script.exhausted();
    }

    #[tokio::test]
    async fn a_registered_task_broadcasts_exact_bytes_and_returns_the_local_id() {
        let transaction = signed(&key(7), 3, 11);
        let script = Script::exchanges([
            height(json!(44)),
            send(&transaction, json!(transaction.id.to_string())),
        ]);
        let registrar = Registrar::accepting();
        let outbox = Arc::new(outbox(&script, registrar.clone()));
        let (prepared, admission) = prepared(&outbox, vec![transaction.clone()]);
        let submitting = outbox.clone();
        let waiter = tokio::spawn(async move { submitting.submit(prepared, admission).await });
        tokio::task::yield_now().await;
        registrar.task().run().await;
        let accepted = waiter.await.unwrap().expect("accepted");
        assert_eq!(accepted, [base::TransactionId::from(transaction.id)]);
        assert!(!busy(&outbox, key(7).address()));
        script.exhausted();
    }

    #[tokio::test]
    async fn bounded_identical_replay_hands_the_unknown_outcome_to_recovery() {
        let (first, second) = (signed(&key(7), 3, 11), signed(&key(7), 4, 11));
        let mismatch = json!(Signature::from([8; 64]).to_string());
        let absent = json!({"context":{"slot":15},"value":[null]});
        let mut exchanges = vec![
            height(json!(44)),
            send(&first, json!(first.id.to_string())),
            height(json!(44)),
        ];
        for attempt in 0..3 {
            exchanges.push(send(&second, mismatch.clone()));
            exchanges.push(status(&second, absent.clone()));
            if attempt < 2 {
                exchanges.push(height(json!(44)));
            }
        }
        let script = Script::exchanges(exchanges);
        let registrar = Registrar::accepting();
        let resolution = Arc::new(Resolution::default());
        let outbox = Arc::new(build(
            script.client(),
            registrar.clone(),
            resolution.clone(),
        ));
        let (prepared, admission) = prepared(&outbox, vec![first.clone(), second.clone()]);
        let submitting = outbox.clone();
        let waiter = tokio::spawn(async move { submitting.submit(prepared, admission).await });
        tokio::task::yield_now().await;
        registrar.task().run().await;
        let error = waiter.await.unwrap().expect_err("unknown outcome");
        let local = base::TransactionId::from(second.id);
        assert_eq!(error.failed_index, Some(1));
        assert_eq!(error.accepted, [base::TransactionId::from(first.id)]);
        assert_eq!(error.ambiguous_transaction_id, Some(local));
        assert_eq!(
            (error.source.kind, error.source.message.as_str()),
            (
                WalletErrorKind::Unavailable,
                "Solana submission outcome is unknown"
            )
        );
        assert_eq!(error.source.ambiguous_transaction_id, None);
        assert_eq!(*resolution.0.lock().unwrap(), [second]);
        assert!(busy(&outbox, key(7).address()), "retained for recovery");
        script.exhausted();
    }

    #[tokio::test]
    async fn a_failed_dispatch_needs_a_valid_status_and_a_live_height_before_replay() {
        let absent = json!({"context":{"slot":15},"value":[null]});
        let landed = json!({"context": {"slot": 15}, "value": [{
            "slot": 12, "confirmations": null,
            "err": {"InstructionError": [0, "Custom"]}, "confirmationStatus": "finalized",
        }]});
        for (status_value, height_value, observed) in [
            (landed, None, true),
            (json!({"context":{"slot":10},"value":[null]}), None, false),
            (absent.clone(), Some(json!(45)), false),
            (absent, Some(json!("unavailable")), false),
        ] {
            let transaction = signed(&key(7), 3, 11);
            let mut exchanges = vec![
                send(&transaction, json!(Signature::from([8; 64]).to_string())),
                status(&transaction, status_value),
            ];
            exchanges.extend(height_value.map(height));
            let script = Script::exchanges(exchanges);
            let outbox = outbox(&script, Registrar::closed());
            let (prepared, admission) = prepared(&outbox, vec![transaction.clone()]);
            let dispatch = Dispatch {
                submitter: outbox.submitter.clone(),
                min_slot: prepared.min_slot,
            };
            match dispatch
                .run(prepared.transactions, admission.settlement())
                .await
            {
                Outcome::Complete(result) => {
                    assert!(observed);
                    assert_eq!(result.unwrap(), [base::TransactionId::from(transaction.id)]);
                }
                Outcome::Unknown {
                    error,
                    transaction: retained,
                } => {
                    assert!(!observed);
                    assert_eq!(*retained, transaction);
                    assert_eq!(error.failed_index, Some(0));
                    assert_eq!(
                        error.ambiguous_transaction_id,
                        Some(base::TransactionId::from(transaction.id))
                    );
                }
            }
            assert_eq!(busy(&outbox, key(7).address()), !observed);
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn a_later_lifetime_failure_keeps_the_accepted_prefix_and_releases_senders() {
        for (value, message) in [
            (
                json!(45),
                "Solana transaction lifetime expired before dispatch",
            ),
            (
                json!("unavailable"),
                "Solana block height is unavailable before dispatch",
            ),
        ] {
            let (first, second) = (signed(&key(7), 3, 11), signed(&key(7), 4, 11));
            let script =
                Script::exchanges([send(&first, json!(first.id.to_string())), height(value)]);
            let outbox = outbox(&script, Registrar::closed());
            let (prepared, admission) = prepared(&outbox, vec![first.clone(), second]);
            let dispatch = Dispatch {
                submitter: outbox.submitter.clone(),
                min_slot: prepared.min_slot,
            };
            let Outcome::Complete(result) = dispatch
                .run(prepared.transactions, admission.settlement())
                .await
            else {
                panic!("an undispatched second item has no unknown outcome");
            };
            let error = result.unwrap_err();
            assert_eq!(error.failed_index, Some(1));
            assert_eq!(error.accepted, [base::TransactionId::from(first.id)]);
            assert_eq!(
                (error.source.kind, error.source.message.as_str()),
                (WalletErrorKind::Unavailable, message)
            );
            assert!(error.ambiguous_transaction_id.is_none());
            assert!(!busy(&outbox, key(7).address()));
            script.exhausted();
        }
    }

    #[tokio::test]
    async fn dropping_the_waiter_after_registration_leaves_the_task_running() {
        let transaction = signed(&key(7), 3, 11);
        let script = Script::exchanges([
            height(json!(44)),
            send(&transaction, json!(transaction.id.to_string())),
        ]);
        let registrar = Registrar::accepting();
        let outbox = Arc::new(outbox(&script, registrar.clone()));
        let (prepared, admission) = prepared(&outbox, vec![transaction]);
        let submitting = outbox.clone();
        let waiter = tokio::spawn(async move { submitting.submit(prepared, admission).await });
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(waiter.await.is_err());
        registrar.task().run().await;
        assert!(!busy(&outbox, key(7).address()));
        script.exhausted();
    }

    #[tokio::test]
    async fn signing_holds_one_transaction_for_exactly_one_broadcast() {
        let node = Node::default();
        let registrar = Registrar::accepting();
        let outbox = Arc::new(build(
            Client::unverified(node.clone(), Script::network()),
            registrar.clone(),
            Arc::new(Resolution::default()),
        ));
        let signer = key(7);
        let (signed, fee) = outbox
            .sign(item(&signer, key(8).address(), 10))
            .await
            .expect("signed");
        assert_eq!(fee, Lamport::from_atomic(5_000));
        assert!(
            !busy(&outbox, signer.address()),
            "signing releases the sender"
        );
        for (id, envelope) in [
            (signed.id, Envelope::new(vec![1])),
            (signed_by(9).id, signed.envelope.clone()),
        ] {
            let error = outbox.broadcast(&id, &envelope).await.unwrap_err();
            assert_eq!(error.kind, base::TransactionErrorKind::InvalidTransaction);
            assert_eq!(error.ambiguous_transaction_id, None);
        }
        assert!(
            !node
                .methods
                .lock()
                .unwrap()
                .contains(&"sendTransaction".to_owned())
        );

        let broadcasting = outbox.clone();
        let held = signed.clone();
        let waiter =
            tokio::spawn(async move { broadcasting.broadcast(&held.id, &held.envelope).await });
        tokio::task::yield_now().await;
        registrar.task().run().await;
        assert_eq!(
            waiter.await.unwrap().expect("broadcast"),
            base::TransactionId::from(signed.id)
        );
        let again = outbox.broadcast(&signed.id, &signed.envelope).await;
        assert_eq!(
            again.unwrap_err().kind,
            base::TransactionErrorKind::InvalidTransaction
        );
    }

    fn signed_by(byte: u8) -> SignedTransaction {
        signed(&key(byte), 5, 11)
    }
}
