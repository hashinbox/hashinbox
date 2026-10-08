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
            let available = item.remainder(funds, balance, &Wei::ZERO)?;
            // The configured ceiling is a limit, not an expected fee. Quote
            // the smallest positive value, then validate the actual remainder.
            item.transfer.amount = match item.transfer.asset {
                AssetKind::Native => Wei::from_u128(1),
                AssetKind::Erc20(_) => available,
            };
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
