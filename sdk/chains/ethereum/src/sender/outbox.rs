//! [`Outbox`]: every send of one factory's wallets and its siblings', in
//! five stages: reserve → prepare → sign → submit → settle.
//!
//! - **reserve** the senders, waiting while one is busy, and recover a
//!   sender's unknown outcome before anything else,
//! - **prepare**: one nonce range per sender from `max(pending, floor)`, a
//!   [`Preflight`] context per transfer, and the batch's aggregate [`Funds`],
//! - **sign** each transfer with its sender's signer and keep the exact
//!   envelopes in the reservation,
//! - **submit** the envelopes in order and stop at the first failure,
//! - **settle**: acceptance raises the sender's nonce floor, a definite
//!   refusal frees the nonce, and an unknown outcome keeps the envelope
//!   for [`super::recovery::Recovery`].

use std::{collections::BTreeMap, sync::Arc};

use base::{Signer, TransactionError};
use wallets::SendError;

use super::{
    dispatch::Dispatch,
    recovery::Recovery,
    reservation::{Admission, AdmissionOutcome, PreparedTransaction, Reservations},
};
use crate::native::{
    Address, Error, ErrorKind, SignedTransaction, TransactionId,
    rpc::{Accounts, Submitter},
    transaction::{BoundTransfer, BuildContext, FeeLimits, Funds, Nonces, Preflight, Transfer},
};

/// The send pipeline of one factory and its siblings: the only place that
/// reserves nonces, signs, and submits.
pub(crate) struct Outbox {
    chain_id: u64,
    accounts: Arc<dyn Accounts>,
    preflight: Arc<dyn Preflight>,
    dispatch: Dispatch,
    recovery: Recovery,
    reservations: Reservations,
}

/// One transfer, its sender, the fee limits it is built within, and the
/// signer that controls the sender.
#[derive(Clone)]
pub(crate) struct SendRequest {
    from: Address,
    transfer: Transfer,
    limits: FeeLimits,
    signer: Arc<dyn Signer>,
    sweep: bool,
}

impl SendRequest {
    /// `transfer` from `from`, signed by `signer`, within `limits`.
    pub(crate) fn new(
        signer: Arc<dyn Signer>,
        from: Address,
        transfer: Transfer,
        limits: FeeLimits,
    ) -> Self {
        Self {
            from,
            transfer,
            limits,
            signer,
            sweep: false,
        }
    }
}

impl SendRequest {
    /// A sweep whose exact amount is resolved while its sender is reserved.
    pub(crate) fn sweep(
        signer: Arc<dyn Signer>,
        from: Address,
        to: Address,
        asset: crate::native::AssetKind,
        limits: FeeLimits,
    ) -> Self {
        Self {
            from,
            transfer: Transfer {
                asset,
                to,
                amount: crate::native::Wei::ZERO,
            },
            limits,
            signer,
            sweep: true,
        }
    }
}

impl SendRequest {
    /// A positive remainder of this holding after earlier items and this fee.
    fn remainder(
        &self,
        funds: &Funds,
        balance: &crate::native::Wei,
        fee: &crate::native::Wei,
    ) -> Result<crate::native::Wei, Error> {
        funds
            .remaining(&self.from, &self.transfer.asset, balance)
            .and_then(|amount| amount.checked_sub(fee))
            .filter(|amount| amount != &crate::native::Wei::ZERO)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InsufficientFunds,
                    "Ethereum sweep has no available funds after transfers and maximum fees",
                )
            })
    }
}

impl Outbox {
    /// A pipeline for chain `chain_id` over the node reads, preflight
    /// policy, and submission it needs, with no reservations yet.
    pub(crate) fn new(
        chain_id: u64,
        accounts: Arc<dyn Accounts>,
        preflight: Arc<dyn Preflight>,
        submitter: Arc<dyn Submitter>,
    ) -> Self {
        let dispatch = Dispatch {
            submitter: submitter.clone(),
        };
        Self {
            chain_id,
            accounts,
            preflight,
            recovery: Recovery {
                submitter,
                dispatch: dispatch.clone(),
            },
            dispatch,
            reservations: Reservations::default(),
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
        let admission = self.reserve(&items).await.map_err(unsubmitted)?;
        let batch = self.prepare(&items).await.map_err(|(index, error)| {
            if items[index].sweep && error.kind == ErrorKind::InsufficientFunds {
                return SendError::item(
                    index,
                    Vec::new(),
                    wallets::WalletError::new(
                        wallets::WalletErrorKind::InvalidAmount,
                        error.message,
                    ),
                );
            }
            unsubmitted((index, error))
        })?;
        self.reservations
            .register(&admission, &batch)
            .map_err(|error| unsubmitted((0, error)))?;
        let mut accepted = Vec::with_capacity(batch.len());
        for prepared in &batch {
            let id = self
                .submit(&prepared.transaction.id, Some(&prepared.transaction))
                .await
                .map_err(|error| SendError::item(accepted.len(), accepted.clone(), error.into()))?;
            accepted.push(id.into());
        }
        drop(admission);
        Ok(accepted)
    }

    /// Reserves, prepares, and signs one transfer, then releases the sender
    /// and holds the transaction for [`Self::broadcast`]. A held transaction
    /// never blocks its sender; if a later send takes its nonce first, its
    /// broadcast fails definitely.
    ///
    /// Returns the signed transaction and the sender nonce it took, which is
    /// what a caller comparing two signed transactions needs and cannot read
    /// off the envelope.
    pub(crate) async fn sign(
        &self,
        item: SendRequest,
    ) -> Result<(SignedTransaction, u64), TransactionError> {
        let items = [item];
        let _admission = self.reserve(&items).await.map_err(|(_, error)| error)?;
        let mut batch = self.prepare(&items).await.map_err(|(_, error)| error)?;
        let prepared = batch.pop().ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                "Ethereum single preparation produced no transaction",
            )
        })?;
        let (transaction, nonce) = (prepared.transaction.clone(), prepared.nonce);
        self.reservations.hold(prepared);
        Ok((transaction, nonce))
    }

    /// Submits and settles a transaction [`Self::sign`] produced, or settles
    /// one whose earlier broadcast had an unknown outcome.
    ///
    /// # Errors
    ///
    /// A definite failure when this outbox holds no exact `transaction`, or
    /// its nonce was taken; otherwise the submission's outcome.
    pub(crate) async fn broadcast(
        &self,
        transaction: SignedTransaction,
    ) -> Result<TransactionId, TransactionError> {
        if let Some(prepared) = self.reservations.take(&transaction) {
            let senders = [(prepared.from.clone(), 0)];
            let admission = self.admit(&senders).await.map_err(|(_, error)| error)?;
            self.reservations
                .register(&admission, std::slice::from_ref(&prepared))
                .map_err(Error::definite)?;
            return self.submit(&transaction.id, Some(&transaction)).await;
        }
        self.submit(&transaction.id, Some(&transaction)).await
    }

    /// Reserve: every item's sender, in first-seen order.
    async fn reserve(&self, items: &[SendRequest]) -> Result<Admission, (usize, Error)> {
        self.admit(&senders(items)).await
    }

    async fn admit(&self, senders: &[(Address, usize)]) -> Result<Admission, (usize, Error)> {
        loop {
            let (id, index) = match self.reservations.admit(senders).await? {
                AdmissionOutcome::Admitted(admission) => return Ok(admission),
                AdmissionOutcome::Recover { id, index } => (id, index),
            };
            self.submit(&id, None).await.map_err(|error| {
                let message =
                    format!("Ethereum sender is blocked by an ambiguous transaction: {error}");
                (index, Error::new(ErrorKind::RpcUnavailable, message))
            })?;
        }
    }

    /// Prepare and sign: nothing leaves the process until every item passed.
    async fn prepare(
        &self,
        items: &[SendRequest],
    ) -> Result<Vec<PreparedTransaction>, (usize, Error)> {
        let nonces = self.nonces(items).await?;
        let mut resolved = items.to_vec();
        let mut contexts = Vec::with_capacity(items.len());
        let mut funds = Funds::default();
        for (index, (item, nonce)) in resolved.iter_mut().zip(nonces).enumerate() {
            let context = self
                .resolve(item, nonce, &funds)
                .await
                .map_err(|error| (index, error))?;
            funds
                .require(index, &item.transfer, &context)
                .map_err(|error| (index, error))?;
            contexts.push(context);
        }
        funds.afford(self.accounts.as_ref()).await?;
        Self::sign_each(&resolved, contexts).await
    }

    /// Quotes an affordable positive transfer, resolves its remainder, and
    /// checks the destination's actual value-dependent execution before signing.
    async fn resolve(
        &self,
        item: &mut SendRequest,
        nonce: u64,
        funds: &Funds,
    ) -> Result<BuildContext, Error> {
        use crate::native::{AssetKind, Wei, rpc::View};
        let balance = if item.sweep {
            Some(
                self.accounts
                    .balance(item.from.clone(), &item.transfer.asset, View::Pending)
                    .await?,
            )
        } else {
            None
        };
        if let Some(balance) = &balance {
            let ceiling = match item.transfer.asset {
                AssetKind::Native => item.limits.max_total_fee(),
                AssetKind::Erc20(_) => &Wei::ZERO,
            };
            item.transfer.amount = item.remainder(funds, balance, ceiling)?;
        }
        let context = self.context(item, nonce).await?;
        if let Some(balance) = balance
            && item.transfer.asset == AssetKind::Native
        {
            let fee = context
                .max_fee_per_gas
                .checked_mul_u64(context.gas_limit)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::FeeUnavailable,
                        "Ethereum maximum fee overflows U256",
                    )
                })?;
            item.transfer.amount = item.remainder(funds, &balance, &fee)?;
            self.validate_sweep(item, nonce, &context).await?;
        }
        Ok(context)
    }

    /// The final value must execute inside the already reserved gas budget.
    /// No zero-value destination assumption or unbounded quote loop is needed.
    async fn validate_sweep(
        &self,
        item: &SendRequest,
        nonce: u64,
        budget: &BuildContext,
    ) -> Result<(), Error> {
        let checked = self.context(item, nonce).await?;
        if checked.gas_limit > budget.gas_limit
            || checked.max_fee_per_gas > budget.max_fee_per_gas
            || checked.max_priority_fee_per_gas > budget.max_priority_fee_per_gas
        {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Ethereum sweep execution exceeds its reserved maximum fee budget",
            ));
        }
        Ok(())
    }

    /// One nonce per item: each sender's range starts at the later of its
    /// pending nonce and its floor. Senders are read in first-seen order.
    async fn nonces(&self, items: &[SendRequest]) -> Result<Vec<u64>, (usize, Error)> {
        let mut ranges = BTreeMap::new();
        for (from, first) in senders(items) {
            let count = items.iter().filter(|item| item.from == from).count();
            let pending = self
                .accounts
                .nonce(from.clone())
                .await
                .map_err(|error| (first, error))?;
            let range = Nonces::reserve(pending, self.reservations.floor(&from), count)
                .map_err(|error| (first, error))?;
            ranges.insert(from, range);
        }
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let range = ranges.get_mut(&item.from).ok_or_else(|| {
                    let error = Error::new(ErrorKind::Other, "Ethereum sender has no nonce range");
                    (index, error)
                })?;
                range.take().map_err(|error| (index, error))
            })
            .collect()
    }

    async fn context(&self, item: &SendRequest, nonce: u64) -> Result<BuildContext, Error> {
        let context = self
            .preflight
            .context(&item.from, &item.transfer, nonce, &item.limits)
            .await?;
        if context.chain_id != self.chain_id {
            return Err(Error::new(
                ErrorKind::Divergent,
                "Ethereum RPC chain ID does not match the wallet network",
            ));
        }
        Ok(context)
    }

    async fn sign_each(
        items: &[SendRequest],
        contexts: Vec<BuildContext>,
    ) -> Result<Vec<PreparedTransaction>, (usize, Error)> {
        let mut batch = Vec::with_capacity(items.len());
        for (index, (item, context)) in items.iter().zip(contexts).enumerate() {
            let nonce = context.nonce;
            let transaction = BoundTransfer::new(item.transfer.clone(), context)
                .sign(item.signer.as_ref())
                .await
                .map_err(|error| (index, error))?;
            batch.push(PreparedTransaction {
                from: item.from.clone(),
                nonce,
                transaction,
            });
        }
        Ok(batch)
    }

    /// Submit and settle the registered transaction `id`: a recovery when
    /// its earlier outcome is unknown, otherwise its first broadcast.
    async fn submit(
        &self,
        id: &TransactionId,
        expected: Option<&SignedTransaction>,
    ) -> Result<TransactionId, TransactionError> {
        let settlement = self
            .reservations
            .settlement(id, expected)
            .await
            .map_err(Error::definite)?;
        if settlement.unknown() {
            return self.recovery.settle(settlement).await;
        }
        self.dispatch.run(settlement).await
    }
}

/// Each distinct sender of `items` with its first item, in first-seen order.
fn senders(items: &[SendRequest]) -> Vec<(Address, usize)> {
    let mut senders: Vec<(Address, usize)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !senders.iter().any(|(from, _)| from == &item.from) {
            senders.push((item.from.clone(), index));
        }
    }
    senders
}

/// A failure of item `index` before anything was submitted.
fn unsubmitted((index, error): (usize, Error)) -> SendError {
    SendError::item(index, Vec::new(), TransactionError::from(error).into())
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use base::{SignFuture, SignRequest};
    use wallets::WalletErrorKind;

    use super::*;
    use crate::native::{AssetKind, Wei};
    use crate::sender::stub::{Action, CHAIN_ID, Node, item, native, signer, token};

    struct UnusedSigner;

    impl Signer for UnusedSigner {
        fn sign<'a>(&'a self, _request: SignRequest) -> SignFuture<'a> {
            panic!("an invalid preparation must fail before signing")
        }
    }

    fn unsigned(from: &Address, transfer: Transfer) -> SendRequest {
        SendRequest::new(
            Arc::new(UnusedSigner),
            from.clone(),
            transfer,
            FeeLimits::default(),
        )
    }

    fn sweep_limits() -> FeeLimits {
        FeeLimits::new(
            1024,
            0,
            2,
            Wei::from_u128(1),
            Wei::from_u128(1),
            Wei::from_u128(2),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn sweep_resolves_after_fixed_amounts_and_all_maximum_fees() {
        use alloy_eips::Decodable2718;
        let key = Arc::new(signer(1));
        let node = Arc::new(Node {
            reject_zero: true,
            ..Node::default()
        });
        node.set_balance(key.address(), &AssetKind::Native, 100);
        let sweep = SendRequest::sweep(
            key.clone(),
            key.address().clone(),
            native(1).to,
            AssetKind::Native,
            sweep_limits(),
        );
        let batch = node
            .outbox()
            .prepare(&[item(&key, native(10)), sweep])
            .await
            .unwrap();
        let decoded = alloy_consensus::TxEnvelope::decode_2718_exact(
            batch[1].transaction.envelope.as_bytes(),
        )
        .unwrap();
        let transaction = decoded.as_eip1559().unwrap().tx();
        assert_eq!(transaction.value, alloy_primitives::U256::from(88));
        assert_eq!(transaction.nonce, 1);
        let quoted = node.transfers.lock().unwrap();
        assert_eq!(
            quoted
                .iter()
                .map(|transfer| transfer.amount.clone())
                .collect::<Vec<_>>(),
            [Wei::from_u128(10), Wei::from_u128(87), Wei::from_u128(88)]
        );
        assert!(node.broadcasts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn token_sweep_does_not_subtract_native_gas_from_token_units() {
        let key = Arc::new(signer(1));
        let node = Arc::new(Node::default());
        let contract = Address::from_bytes([9; 20]);
        let asset = AssetKind::Erc20(contract.clone());
        node.set_balance(key.address(), &asset, 100);
        node.set_balance(key.address(), &AssetKind::Native, 2);
        let sweep = SendRequest::sweep(
            key.clone(),
            key.address().clone(),
            native(1).to,
            asset,
            FeeLimits::default(),
        );
        node.outbox()
            .prepare(&[item(&key, token(&contract, 10)), sweep])
            .await
            .unwrap();
        assert_eq!(node.transfers.lock().unwrap()[1].amount, Wei::from_u128(90));
    }

    #[tokio::test]
    async fn sweep_refuses_changed_execution_budget_and_empty_remainder_before_signing() {
        let key = Arc::new(signer(1));
        for balance in [1, 100] {
            let node = Arc::new(Node::default());
            node.set_balance(key.address(), &AssetKind::Native, balance);
            node.gas_limits.lock().unwrap().extend([1, 2]);
            let sweep = SendRequest::sweep(
                Arc::new(UnusedSigner),
                key.address().clone(),
                native(1).to,
                AssetKind::Native,
                sweep_limits(),
            );
            let failure = node.outbox().prepare(&[sweep]).await.err().unwrap();
            assert_eq!(failure.0, 0);
            assert_eq!(
                failure.1.kind,
                if balance == 1 {
                    ErrorKind::InsufficientFunds
                } else {
                    ErrorKind::FeeUnavailable
                }
            );
            assert!(node.broadcasts.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn nonce_overflow_reports_first_occurrence_before_contexts_or_later_sender_reads() {
        let first = Address::from_bytes([0xff; 20]);
        let second = Address::from_bytes([1; 20]);
        for (exhausted, failed_index, expected_reads) in [
            (first.clone(), 0, vec![first.clone()]),
            (second.clone(), 1, vec![first.clone(), second.clone()]),
        ] {
            let node = Arc::new(Node::default());
            node.set_nonce(&exhausted, u64::MAX);
            let items = vec![
                unsigned(&first, native(1)),
                unsigned(&second, native(1)),
                unsigned(&first, native(1)),
            ];
            let (index, error) = node.outbox().prepare(&items).await.err().unwrap();
            assert_eq!(index, failed_index);
            assert_eq!(error.kind, ErrorKind::InvalidTransaction);
            assert_eq!(error.message, "Ethereum batch nonce range overflows u64");
            assert_eq!(*node.nonce_reads.lock().unwrap(), expected_reads);
            assert!(node.balance_reads.lock().unwrap().is_empty());
            assert!(node.contexts.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn context_and_aggregate_failures_precede_signing_and_release_sender_admission() {
        let source = Address::from_bytes([1; 20]);
        let node = Arc::new(Node::default());
        node.set_balance(&source, &AssetKind::Native, 3);
        let outbox = node.outbox();
        for (chain_id, message) in [
            (
                CHAIN_ID + 1,
                "Ethereum RPC chain ID does not match the wallet network",
            ),
            (CHAIN_ID, "Ethereum aggregate batch balance is insufficient"),
        ] {
            node.chain_ids.lock().unwrap().extend([CHAIN_ID, chain_id]);
            let items = vec![unsigned(&source, native(1)), unsigned(&source, native(1))];
            let error = tokio::time::timeout(Duration::from_secs(2), outbox.send(items))
                .await
                .expect("a failed send must release its sender")
                .expect_err("preflight must fail");
            assert_eq!(error.failed_index, Some(1));
            assert_eq!(error.source.kind, WalletErrorKind::Transaction);
            assert_eq!(error.source.message, message);
            let contexts = node.contexts.lock().unwrap().clone();
            assert_eq!(
                &contexts[contexts.len() - 2..],
                &[(source.clone(), 0), (source.clone(), 1)]
            );
            assert_eq!(
                node.balance_reads.lock().unwrap().len(),
                usize::from(chain_id == CHAIN_ID)
            );
            assert!(node.broadcasts.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn prepares_a_b_a_with_consecutive_per_sender_nonces_before_broadcast() {
        let (a, b) = (Arc::new(signer(1)), Arc::new(signer(2)));
        let node = Arc::new(Node::default());
        node.set_nonce(a.address(), 5);
        node.set_nonce(b.address(), 9);
        let outbox = node.outbox();
        let items = vec![
            item(&a, native(1)),
            item(&b, native(2)),
            item(&a, native(3)),
        ];
        let batch = outbox.prepare(&items).await.expect("valid batch");
        assert_eq!(
            *node.contexts.lock().unwrap(),
            [
                (a.address().clone(), 5),
                (b.address().clone(), 9),
                (a.address().clone(), 6)
            ]
        );
        assert_eq!(batch.len(), 3);
        assert!(node.broadcasts.lock().unwrap().is_empty());

        let accepted = outbox.send(items).await.expect("valid batch must send");
        let broadcasts = node.broadcasts.lock().unwrap().clone();
        let ids = broadcasts
            .iter()
            .map(|transaction| base::TransactionId::from(&transaction.id))
            .collect::<Vec<_>>();
        assert_eq!(accepted, ids);
        assert_eq!(broadcasts.len(), 3);
    }

    #[tokio::test]
    async fn aggregate_overspend_reports_first_threshold_crossing_without_broadcast() {
        let key = Arc::new(signer(3));
        let node = Arc::new(Node::default());
        node.set_balance(key.address(), &AssetKind::Native, 10);
        let error = node
            .outbox()
            .send(vec![item(&key, native(4)), item(&key, native(5))])
            .await
            .expect_err("aggregate value plus fees must exceed the balance");
        assert_eq!(error.failed_index, Some(1));
        assert_eq!(
            error.source.message,
            "Ethereum aggregate batch balance is insufficient"
        );
        assert!(node.broadcasts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cumulative_token_amount_and_native_gas_fail_before_any_broadcast() {
        let key = Arc::new(signer(7));
        let contract = Address::from_bytes([0x77; 20]);
        let node = Arc::new(Node::default());
        let outbox = node.outbox();
        for (token_balance, native_balance) in [(10, 2), (12, 1)] {
            node.set_balance(
                key.address(),
                &AssetKind::Erc20(contract.clone()),
                token_balance,
            );
            node.set_balance(key.address(), &AssetKind::Native, native_balance);
            let items = (0..2)
                .map(|_| item(&key, token(&contract, 6)))
                .collect::<Vec<_>>();
            let error = outbox.send(items).await.expect_err("the second must fail");
            assert_eq!(error.failed_index, Some(1));
            assert_eq!(
                error.source.message,
                "Ethereum aggregate batch balance is insufficient"
            );
            assert!(node.broadcasts.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_signed_transaction_never_broadcast_does_not_block_its_sender() {
        let key = Arc::new(signer(11));
        let node = Arc::new(Node::default());
        node.set_nonce(key.address(), 5);
        let outbox = node.outbox();
        let (abandoned, nonce) = outbox
            .sign(item(&key, native(1)))
            .await
            .expect("the single transfer must sign");
        assert_eq!(
            nonce, 5,
            "it reports the nonce it took, and holds none of it"
        );

        let accepted = tokio::time::timeout(
            Duration::from_secs(2),
            outbox.send(vec![item(&key, native(2))]),
        )
        .await
        .expect("an unbroadcast signature must not block the sender")
        .expect("the next send must submit");
        assert_eq!(accepted.len(), 1);
        assert_eq!(node.nonces_used(), [5, 5]);

        let error = outbox
            .broadcast(abandoned)
            .await
            .expect_err("the accepted send took the held nonce");
        assert_eq!(error.kind, base::TransactionErrorKind::Rejected);
        assert_eq!(error.ambiguous_transaction_id, None);
        assert_eq!(node.broadcasts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_held_transaction_broadcasts_once_and_is_then_forgotten() {
        let key = Arc::new(signer(12));
        let node = Arc::new(Node::default());
        node.actions([Action::Accept]);
        let outbox = node.outbox();
        let (signed, _) = outbox.sign(item(&key, native(1))).await.unwrap();
        assert_eq!(
            outbox.broadcast(signed.clone()).await,
            Ok(signed.id.clone())
        );
        let error = outbox.broadcast(signed).await.unwrap_err();
        assert_eq!(error.kind, base::TransactionErrorKind::Rejected);
        assert_eq!(node.broadcasts.lock().unwrap().len(), 1);
    }
}
